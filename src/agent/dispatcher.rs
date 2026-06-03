//! Dispatcher + SubAgentPool — orchestrate concurrent sub-agent execution.
//!
//! ## Layering
//!
//! - [`SubAgentPool`] runs one batch at a time. A batch is a list of
//!   independent `SubTask`s (no cross-task deps among them) — it spawns
//!   each task as a tokio future, throttled by a semaphore, and joins them.
//! - [`Dispatcher`] runs an entire task graph: it asks the
//!   [`DagScheduler`](crate::scheduler::DagScheduler) for wave-aligned
//!   batches and feeds each batch into the pool in order, accumulating
//!   results.
//!
//! ## Scope (Phase 3 v1)
//!
//! - All batches share one `SubAgent` instance — same model, same
//!   parameters. Phase 4 will let the dispatcher pick different agents per
//!   task `needs` (e.g. read-only tasks → fast tier; write tasks → deep).
//! - A sub-agent failure (model error captured in `SubAgentResult`) does
//!   not abort the batch; we collect all results, success or not. The
//!   caller decides what to do with failures. We DO abort on a tokio join
//!   error (panic in the spawned task) — that's a bug, not a sub-agent
//!   problem.
//! - No retry, no backoff, no cancellation propagation between siblings.
//!   Each task is single-shot.

use super::graph_loop::GraphError;
use super::subagent::{SubAgent, SubAgentResult, SubTask};
use crate::context::SourceLoader;
use crate::error::{HarnessError, Result};
use crate::graph::{Graph, NodeId};
use crate::scheduler::DagScheduler;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// SubAgentPool
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SubAgentPool {
    pub agent: Arc<SubAgent>,
    pub max_concurrent: usize,
    /// When true (the default), every spawned sub-agent is given a
    /// `ScopeGuard` derived from its task's `involved_nodes`. Set to
    /// false if the caller is providing scope at a different layer.
    pub auto_scope: bool,
}

impl SubAgentPool {
    pub fn new(agent: Arc<SubAgent>, max_concurrent: usize) -> Self {
        Self {
            agent,
            max_concurrent: max_concurrent.max(1),
            auto_scope: true,
        }
    }

    /// Toggle the pool's auto-scope behavior. When false, the pool will
    /// NOT install a per-task `ScopeGuard` — useful for tests or callers
    /// that manage scope at a higher layer.
    pub fn with_auto_scope(mut self, yes: bool) -> Self {
        self.auto_scope = yes;
        self
    }

    /// Execute one batch concurrently. The batch is `Vec<NodeId>` —
    /// indices into the task graph; the pool fetches the corresponding
    /// `Node`s and converts to `SubTask` internally.
    ///
    /// Returns results in the same order as the input batch (the pool
    /// preserves order even though spawn order is non-deterministic).
    pub async fn run_batch(
        &self,
        batch: &[NodeId],
        task_graph: Arc<Graph>,
        world_graph: Arc<Graph>,
        loader: Arc<dyn SourceLoader>,
    ) -> Result<Vec<SubAgentResult>> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        let semaphore = Arc::new(Semaphore::new(self.max_concurrent));
        let mut handles = Vec::with_capacity(batch.len());

        let auto_scope = self.auto_scope;
        for task_id in batch.iter().cloned() {
            let agent = self.agent.clone();
            let task_graph = task_graph.clone();
            let world_graph = world_graph.clone();
            let loader = loader.clone();
            let sem = semaphore.clone();

            let handle = tokio::spawn(async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .map_err(|e| HarnessError::domain(format!("semaphore closed: {e}")))?;
                let node = task_graph.get_node(&task_id).ok_or_else(|| {
                    HarnessError::domain(format!(
                        "pool: task id {task_id} not found in task graph"
                    ))
                })?;
                let sub_task = SubTask::from_task_node(node)?;
                let agent = if auto_scope {
                    let guard = std::sync::Arc::new(
                        crate::tools::ScopeGuard::from_involved_nodes(
                            world_graph.as_ref(),
                            &sub_task.involved_nodes,
                        ),
                    );
                    agent.with_task_scope(guard)
                } else {
                    agent.as_ref().clone()
                };
                agent.execute(&sub_task, &world_graph, loader.as_ref()).await
            });
            handles.push(handle);
        }

        let mut results = Vec::with_capacity(handles.len());
        for h in handles {
            match h.await {
                Ok(Ok(r)) => results.push(r),
                Ok(Err(e)) => return Err(e),
                Err(join_err) => {
                    return Err(HarnessError::domain(format!(
                        "pool: sub-agent join failed (panic in spawned task?): {join_err}"
                    )));
                }
            }
        }
        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct DispatcherConfig {
    pub max_concurrent: usize,
    /// Optional per-batch wall-clock cap. `None` = unlimited.
    pub batch_timeout: Option<Duration>,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            batch_timeout: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DispatchOutcome {
    /// Results in topological order. Each entry corresponds to one task
    /// node from the input task graph.
    pub results: Vec<SubAgentResult>,
    /// The schedule used (for audit / logs).
    pub batches: Vec<Vec<NodeId>>,
    /// Sum of `duration_ms` across all results.
    pub total_subagent_ms: u64,
    /// Sum of `tokens_used` across all results.
    pub total_tokens: usize,
    pub all_succeeded: bool,
    /// Aggregated graph errors reported by sub-agents during execution.
    /// When non-empty, the GraphLoop surfaces `LoopState::GraphInvalid {
    /// source: DuringExecution }` instead of advancing to Review — the
    /// caller is expected to repair the graph and resume.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub graph_errors: Vec<GraphError>,
}

#[derive(Clone)]
pub struct Dispatcher {
    pub pool: SubAgentPool,
    pub scheduler: DagScheduler,
    pub config: DispatcherConfig,
}

impl Dispatcher {
    pub fn new(agent: Arc<SubAgent>) -> Self {
        let cfg = DispatcherConfig::default();
        Self {
            pool: SubAgentPool::new(agent, cfg.max_concurrent),
            scheduler: DagScheduler::new(),
            config: cfg,
        }
    }

    pub fn with_max_concurrent(mut self, n: usize) -> Self {
        let n = n.max(1);
        self.config.max_concurrent = n;
        self.pool.max_concurrent = n;
        self
    }

    pub fn with_batch_timeout(mut self, d: Duration) -> Self {
        self.config.batch_timeout = Some(d);
        self
    }

    /// Run the entire task graph: schedule → execute each batch → aggregate.
    pub async fn run(
        &self,
        task_graph: &Graph,
        world_graph: &Graph,
        loader: Arc<dyn SourceLoader>,
    ) -> Result<DispatchOutcome> {
        let started = Instant::now();
        let schedule = self.scheduler.plan(task_graph)?;
        info!(
            tasks = schedule.task_count(),
            batches = schedule.depth(),
            "dispatcher scheduled task graph"
        );

        let task_graph = Arc::new(task_graph.clone());
        let world_graph = Arc::new(world_graph.clone());

        let mut all_results: Vec<SubAgentResult> = Vec::new();
        for (batch_idx, batch) in schedule.batches.iter().enumerate() {
            let batch_started = Instant::now();
            let fut = self.pool.run_batch(
                batch,
                task_graph.clone(),
                world_graph.clone(),
                loader.clone(),
            );
            let batch_results = match self.config.batch_timeout {
                Some(t) => match tokio::time::timeout(t, fut).await {
                    Ok(r) => r?,
                    Err(_) => {
                        warn!(batch = batch_idx, timeout_ms = t.as_millis() as u64, "dispatcher batch timed out");
                        return Err(HarnessError::domain(format!(
                            "dispatcher: batch {batch_idx} timed out after {:?}",
                            t
                        )));
                    }
                },
                None => fut.await?,
            };
            debug!(
                batch = batch_idx,
                size = batch.len(),
                duration_ms = batch_started.elapsed().as_millis() as u64,
                "dispatcher batch complete"
            );
            all_results.extend(batch_results);
        }

        let total_subagent_ms: u64 = all_results.iter().map(|r| r.duration_ms).sum();
        let total_tokens: usize = all_results.iter().map(|r| r.tokens_used).sum();
        // Aggregate graph errors across all sub-agents — these flow up to
        // the GraphLoop for repair.
        let graph_errors: Vec<GraphError> = all_results
            .iter()
            .flat_map(|r| r.graph_errors.iter().cloned())
            .collect();
        // A run "succeeded" iff every sub-agent succeeded AND no sub-agent
        // bubbled a graph error.
        let all_succeeded = graph_errors.is_empty() && all_results.iter().all(|r| r.success);

        let outcome = DispatchOutcome {
            results: all_results,
            batches: schedule.batches.clone(),
            total_subagent_ms,
            total_tokens,
            all_succeeded,
            graph_errors,
        };
        info!(
            wall_ms = started.elapsed().as_millis() as u64,
            subagent_ms = outcome.total_subagent_ms,
            tokens = outcome.total_tokens,
            success = outcome.all_succeeded,
            graph_errors = outcome.graph_errors.len(),
            "dispatcher complete"
        );
        Ok(outcome)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::InMemorySources;
    use crate::domain::TaskNeeds;
    use crate::graph::{Edge, Node, RelationType};
    use crate::model::{FinishReason, Model, ModelRequest, ModelResponse, Usage};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Model that returns a unique response per call and tracks concurrent
    /// in-flight calls so tests can assert the pool actually runs in parallel.
    struct CountingModel {
        responses: Mutex<Vec<String>>,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
        call_delay_ms: u64,
    }

    impl CountingModel {
        fn new(responses: Vec<&str>, call_delay_ms: u64) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().rev().map(String::from).collect()),
                in_flight: Arc::new(AtomicUsize::new(0)),
                max_in_flight: Arc::new(AtomicUsize::new(0)),
                call_delay_ms,
            }
        }
    }

    #[async_trait]
    impl Model for CountingModel {
        fn name(&self) -> &str {
            "counting"
        }
        async fn complete(&self, _: ModelRequest) -> Result<ModelResponse> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            if self.call_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.call_delay_ms)).await;
            }
            let content = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| "ok".to_string());
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(ModelResponse {
                content,
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                usage: Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                },
            })
        }
    }

    fn make_world() -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::file("a", "A"));
        g.add_node(Node::file("b", "B"));
        g.add_node(Node::file("c", "C"));
        g
    }

    fn make_task_graph_diamond() -> Graph {
        // t1, t2 (independent) -> t3 (depends on both)
        let mut g = Graph::new();
        let st1 = SubTask {
            id: NodeId::from("t1"),
            description: "analyze A".into(),
            involved_nodes: vec![NodeId::from("a")],
            needs: TaskNeeds::read_only(),
        };
        let st2 = SubTask {
            id: NodeId::from("t2"),
            description: "analyze B".into(),
            involved_nodes: vec![NodeId::from("b")],
            needs: TaskNeeds::read_only(),
        };
        let st3 = SubTask {
            id: NodeId::from("t3"),
            description: "synthesize".into(),
            involved_nodes: vec![NodeId::from("a"), NodeId::from("b"), NodeId::from("c")],
            needs: TaskNeeds::read_only(),
        };
        g.add_node(st1.to_task_node());
        g.add_node(st2.to_task_node());
        g.add_node(st3.to_task_node());
        g.add_edge(Edge::new("t3", "t1", RelationType::DependsOn, 1.0, "")).unwrap();
        g.add_edge(Edge::new("t3", "t2", RelationType::DependsOn, 1.0, "")).unwrap();
        g
    }

    fn loader() -> Arc<dyn SourceLoader> {
        Arc::new(InMemorySources(HashMap::new()))
    }

    #[tokio::test]
    async fn dispatcher_runs_diamond_in_two_waves() {
        let model: Arc<dyn Model> =
            Arc::new(CountingModel::new(vec!["A done", "B done", "synth done"], 0));
        let agent = Arc::new(SubAgent::new(model));
        let d = Dispatcher::new(agent).with_max_concurrent(4);
        let outcome = d
            .run(&make_task_graph_diamond(), &make_world(), loader())
            .await
            .unwrap();
        assert_eq!(outcome.results.len(), 3);
        assert!(outcome.all_succeeded);
        // Schedule should have produced 2 batches: [t1,t2] then [t3]
        assert_eq!(outcome.batches.len(), 2);
        assert_eq!(outcome.batches[0].len(), 2);
        assert_eq!(outcome.batches[1].len(), 1);
    }

    #[tokio::test]
    async fn pool_actually_runs_batch_concurrently() {
        // Two sleeping calls — if executed serially, total ≈ 200ms.
        // With max_concurrent=2 and pool, both run in parallel, total ≈ 100ms.
        let mc = CountingModel::new(vec!["x", "y"], 100);
        let in_flight = mc.in_flight.clone();
        let max_in_flight = mc.max_in_flight.clone();
        let model: Arc<dyn Model> = Arc::new(mc);
        let agent = Arc::new(SubAgent::new(model));
        let pool = SubAgentPool::new(agent, 2);

        let mut g = Graph::new();
        g.add_node(SubTask {
            id: NodeId::from("ta"),
            description: "".into(),
            involved_nodes: vec![],
            needs: TaskNeeds::default(),
        }.to_task_node());
        g.add_node(SubTask {
            id: NodeId::from("tb"),
            description: "".into(),
            involved_nodes: vec![],
            needs: TaskNeeds::default(),
        }.to_task_node());

        let started = Instant::now();
        let r = pool
            .run_batch(
                &[NodeId::from("ta"), NodeId::from("tb")],
                Arc::new(g),
                Arc::new(Graph::new()),
                loader(),
            )
            .await
            .unwrap();
        let elapsed_ms = started.elapsed().as_millis() as u64;

        assert_eq!(r.len(), 2);
        // Max concurrent calls must have reached 2 (proves parallelism)
        assert_eq!(max_in_flight.load(Ordering::SeqCst), 2);
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
        // Wall time should be ~100ms, not ~200ms (give generous slack)
        assert!(elapsed_ms < 180, "expected parallel execution, took {}ms", elapsed_ms);
    }

    #[tokio::test]
    async fn pool_respects_max_concurrent_limit() {
        // 4 tasks, max_concurrent=2 → max_in_flight should be 2, not 4
        let mc = CountingModel::new(vec!["a", "b", "c", "d"], 80);
        let max_in_flight = mc.max_in_flight.clone();
        let model: Arc<dyn Model> = Arc::new(mc);
        let agent = Arc::new(SubAgent::new(model));
        let pool = SubAgentPool::new(agent, 2);

        let mut g = Graph::new();
        for id in ["ta", "tb", "tc", "td"] {
            g.add_node(SubTask {
                id: NodeId::from(id),
                description: "".into(),
                involved_nodes: vec![],
                needs: TaskNeeds::default(),
            }.to_task_node());
        }
        let ids: Vec<NodeId> = ["ta", "tb", "tc", "td"].iter().map(|s| NodeId::from(*s)).collect();
        let _ = pool
            .run_batch(&ids, Arc::new(g), Arc::new(Graph::new()), loader())
            .await
            .unwrap();
        // Max in-flight observed should be ≤ max_concurrent
        let max_obs = max_in_flight.load(Ordering::SeqCst);
        assert!(max_obs <= 2, "pool exceeded max_concurrent: saw {max_obs}");
    }

    #[tokio::test]
    async fn empty_task_graph_yields_empty_outcome() {
        let model: Arc<dyn Model> = Arc::new(CountingModel::new(vec![], 0));
        let agent = Arc::new(SubAgent::new(model));
        let d = Dispatcher::new(agent);
        let outcome = d
            .run(&Graph::new(), &make_world(), loader())
            .await
            .unwrap();
        assert!(outcome.results.is_empty());
        assert!(outcome.batches.is_empty());
        assert!(outcome.all_succeeded);
    }

    #[tokio::test]
    async fn outcome_aggregates_tokens_and_durations() {
        let model: Arc<dyn Model> =
            Arc::new(CountingModel::new(vec!["A done", "B done", "synth done"], 5));
        let agent = Arc::new(SubAgent::new(model));
        let d = Dispatcher::new(agent);
        let outcome = d
            .run(&make_task_graph_diamond(), &make_world(), loader())
            .await
            .unwrap();
        // Each sub-agent reports 15 tokens (per CountingModel).
        assert_eq!(outcome.total_tokens, 15 * 3);
        // total_subagent_ms is the SUM of per-task durations, not wall clock —
        // with 5ms calls × 3 tasks, it's at least ~15ms.
        assert!(outcome.total_subagent_ms >= 15);
    }

    #[tokio::test]
    async fn dispatcher_propagates_scheduler_cycle_error() {
        // Build a cyclic task graph manually
        let mut g = Graph::new();
        g.add_node(SubTask {
            id: NodeId::from("t1"),
            description: "x".into(),
            involved_nodes: vec![],
            needs: TaskNeeds::default(),
        }.to_task_node());
        g.add_node(SubTask {
            id: NodeId::from("t2"),
            description: "y".into(),
            involved_nodes: vec![],
            needs: TaskNeeds::default(),
        }.to_task_node());
        g.add_edge(Edge::new("t1", "t2", RelationType::DependsOn, 1.0, "")).unwrap();
        g.add_edge(Edge::new("t2", "t1", RelationType::DependsOn, 1.0, "")).unwrap();

        let model: Arc<dyn Model> = Arc::new(CountingModel::new(vec![], 0));
        let agent = Arc::new(SubAgent::new(model));
        let d = Dispatcher::new(agent);
        let err = d.run(&g, &make_world(), loader()).await.unwrap_err();
        assert!(format!("{err}").contains("cycle"));
    }

    #[tokio::test]
    async fn dispatcher_aggregates_subagent_graph_errors_into_outcome() {
        // Two tasks: t1 emits a graph-error report; t2 emits a normal final_answer.
        // Outcome should have graph_errors populated (from t1) AND all_succeeded=false.
        let report = r#"{"action":"report_graph_error","errors":[{"kind":"L0Structural","l0_error_type":"WrongRelation","detail":"A doesn't call B","related_nodes":["a","b"]}]}"#;
        let normal = r#"{"action":"final_answer","answer":"done","thinking":""}"#;
        let model: Arc<dyn Model> = Arc::new(CountingModel::new(vec![report, normal], 0));
        let agent = Arc::new(SubAgent::new(model));
        let d = Dispatcher::new(agent).with_max_concurrent(2);

        let mut g = Graph::new();
        g.add_node(SubTask {
            id: NodeId::from("t1"),
            description: "x".into(),
            involved_nodes: vec![],
            needs: TaskNeeds::default(),
        }.to_task_node());
        g.add_node(SubTask {
            id: NodeId::from("t2"),
            description: "y".into(),
            involved_nodes: vec![],
            needs: TaskNeeds::default(),
        }.to_task_node());

        let outcome = d.run(&g, &make_world(), loader()).await.unwrap();
        // Two results
        assert_eq!(outcome.results.len(), 2);
        // graph_errors aggregated — exactly one
        assert_eq!(outcome.graph_errors.len(), 1);
        // all_succeeded should be false because t1 reported a graph error
        assert!(!outcome.all_succeeded);
    }

    #[test]
    fn auto_scope_default_is_true() {
        // A fresh pool must default to auto-scope so the safety
        // guarantee is in effect by default.
        let model: Arc<dyn Model> = Arc::new(CountingModel::new(Vec::new(), 0));
        let agent = Arc::new(SubAgent::new(model));
        let pool = SubAgentPool::new(agent, 1);
        assert!(pool.auto_scope);
    }

    #[test]
    fn with_auto_scope_toggles() {
        let model: Arc<dyn Model> = Arc::new(CountingModel::new(Vec::new(), 0));
        let agent = Arc::new(SubAgent::new(model));
        let pool = SubAgentPool::new(agent, 1).with_auto_scope(false);
        assert!(!pool.auto_scope);
    }
}
