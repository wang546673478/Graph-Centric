//! Reviewer — final acceptance gate before Done.
//!
//! Per design principle #5 (deterministic reviewer backstops): an LLM-as-judge
//! alone is unreliable, so the Reviewer layers deterministic checks under any
//! model call.
//!
//! ## Layers
//!
//! 1. **Deterministic** — always on, no model needed:
//!    - **Graph consistency**: re-run [`Graph::find_inconsistencies`].
//!      Failing a structural check means whatever happened earlier didn't
//!      stick (e.g., a repair patch left a dangling edge).
//!    - **Sub-agent success**: every dispatched sub-agent must report
//!      `success=true`. A failed sub-agent fails the review.
//!    - **Verifier-final status**: the most recent Graph-phase verification
//!      must have passed. (Belt-and-suspenders — the loop wouldn't reach
//!      Review otherwise, but cheap to assert.)
//!
//! 2. **LLM-as-judge** (optional) — asks the model:
//!    *Given the task, the final graph, and the sub-agent results, is the
//!    work satisfactory? If not, what's the root cause?*
//!
//! ## Verdict → next phase
//!
//! | Verdict                                        | GraphLoop action                  |
//! |------------------------------------------------|-----------------------------------|
//! | Deterministic + judge pass                     | `Phase::Done`                     |
//! | Deterministic fail OR judge=GraphIssue/ScopeIssue | Surface `LoopState::GraphInvalid { source: Review }` to the caller |
//! | Judge=TaskIssue                                | `Phase::Done` with `passed=false` embedded in `ReviewResult` |

use super::dispatcher::DispatchOutcome;
use super::graph_loop::{GraphError, L0ErrorType};
use super::proposer::extract_json_block;
use super::verifier::VerificationResult;
use crate::domain::CheckResult;
use crate::error::{HarnessError, Result};
use crate::graph::{Graph, NodeId};
use crate::model::{Message, Model, ModelRequest};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// Verdict types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RootCause {
    /// The relationship graph itself is wrong — missing/wrong edges or nodes.
    GraphIssue,
    /// The graph is correct but the sub-agents produced bad work (code-level
    /// failure, missed requirement, etc.). Phase 5 will route back to Task
    /// with feedback; v1 just surfaces Done with the verdict embedded.
    TaskIssue,
    /// The graph's scope is too narrow — needed regions were not modelled.
    /// Routes back to Graph phase for expansion.
    ScopeIssue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeVerdict {
    pub passed: bool,
    /// Only present when `passed=false`. Drives the loop's routing decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_cause: Option<RootCause>,
    pub detail: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewResult {
    pub passed: bool,
    pub deterministic_checks: Vec<CheckResult>,
    /// Present iff the Reviewer was configured with a model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge_verdict: Option<JudgeVerdict>,
    pub rationale: String,
}

impl ReviewResult {
    pub fn root_cause(&self) -> Option<&RootCause> {
        self.judge_verdict.as_ref().and_then(|j| j.root_cause.as_ref())
    }
}

// ---------------------------------------------------------------------------
// Reviewer
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Reviewer {
    /// Optional model for the LLM-as-judge layer. Without it, only
    /// deterministic checks run.
    pub model: Option<Arc<dyn Model>>,
    pub temperature: f64,
    pub max_tokens: Option<usize>,
}

impl Reviewer {
    /// Deterministic-only reviewer. Cheap, model-free, no judgment beyond
    /// "did the structural state stay consistent and did all sub-agents
    /// succeed?" Suitable for sub-agents themselves and for low-stakes runs.
    pub fn deterministic_only() -> Self {
        Self {
            model: None,
            temperature: 0.0,
            max_tokens: Some(1024),
        }
    }

    pub fn with_model(model: Arc<dyn Model>) -> Self {
        Self {
            model: Some(model),
            temperature: 0.0,
            max_tokens: Some(1024),
        }
    }

    /// Run the full review.
    pub async fn review(
        &self,
        task: &str,
        graph: &Graph,
        task_outcome: Option<&DispatchOutcome>,
        last_verification: Option<&VerificationResult>,
    ) -> Result<ReviewResult> {
        let mut checks = Vec::new();

        // ---- Deterministic layer ----
        let inconsistencies = graph.find_inconsistencies();
        let high_or_blocking = inconsistencies.iter().any(|i| {
            matches!(
                i,
                crate::graph::Inconsistency::DanglingEdge { .. }
                    | crate::graph::Inconsistency::Cycle { .. }
            )
        });
        checks.push(CheckResult {
            name: "graph_consistency".into(),
            passed: !high_or_blocking,
            details: if inconsistencies.is_empty() {
                "no inconsistencies".into()
            } else {
                format!(
                    "{} inconsistencies (high-severity: {})",
                    inconsistencies.len(),
                    if high_or_blocking { "yes" } else { "no" }
                )
            },
        });

        if let Some(outcome) = task_outcome {
            let succeeded = outcome.results.iter().filter(|r| r.success).count();
            let total = outcome.results.len();
            checks.push(CheckResult {
                name: "subagent_results".into(),
                passed: outcome.all_succeeded,
                details: format!("{succeeded}/{total} succeeded, all_succeeded={}", outcome.all_succeeded),
            });
            if !outcome.graph_errors.is_empty() {
                checks.push(CheckResult {
                    name: "subagent_graph_errors".into(),
                    passed: false,
                    details: format!(
                        "{} sub-agent(s) reported graph errors",
                        outcome.graph_errors.len()
                    ),
                });
            }
        }

        if let Some(v) = last_verification {
            checks.push(CheckResult {
                name: "last_verification".into(),
                passed: v.passed,
                details: v.rationale.clone(),
            });
        }

        let det_passed = checks.iter().all(|c| c.passed);
        debug!(det_passed, check_count = checks.len(), "reviewer: deterministic layer complete");

        // ---- LLM-as-judge layer ----
        let judge = if let Some(model) = &self.model {
            Some(
                self.judge(model.as_ref(), task, graph, task_outcome)
                    .await?,
            )
        } else {
            None
        };

        let passed = det_passed && judge.as_ref().map(|j| j.passed).unwrap_or(true);
        let rationale = match &judge {
            Some(j) => format!(
                "deterministic={det_passed}, judge={} (root_cause={:?}, confidence={:.2})",
                j.passed, j.root_cause, j.confidence
            ),
            None => format!("deterministic={det_passed}, judge=skipped (no model)"),
        };

        info!(
            passed,
            checks = checks.len(),
            judge_passed = judge.as_ref().map(|j| j.passed),
            "reviewer: complete"
        );
        Ok(ReviewResult {
            passed,
            deterministic_checks: checks,
            judge_verdict: judge,
            rationale,
        })
    }

    async fn judge(
        &self,
        model: &dyn Model,
        task: &str,
        graph: &Graph,
        outcome: Option<&DispatchOutcome>,
    ) -> Result<JudgeVerdict> {
        let graph_sketch = render_graph_for_judge(graph);
        let outcome_block = outcome
            .map(render_outcome_for_judge)
            .unwrap_or_else(|| "(no Task phase ran)".to_string());

        let user_prompt = format!(
            "## Original task\n{task}\n\n## Final relationship graph\n{graph_sketch}\n\n\
             ## Sub-agent outcomes\n{outcome_block}\n\n\
             Decide whether the work satisfactorily addresses the original task. \
             Reply with ONE JSON object only:\n\n\
             {{\n  \"verdict\": \"pass\" | \"fail\",\n  \
             \"root_cause\": \"graph\" | \"task\" | \"scope\" | null,\n  \
             \"detail\": \"<one sentence>\",\n  \"confidence\": 0..1\n}}\n\n\
             Rules:\n\
             - `verdict=pass` when the original task is genuinely addressed by the graph + outcomes.\n\
             - `verdict=fail` only when there's a concrete shortfall you can name.\n\
             - `root_cause`:\n\
               * graph  → the relationship graph is wrong (missing/wrong nodes or edges)\n\
               * task   → the graph is fine but the sub-agents produced bad / incomplete work\n\
               * scope  → the graph's scope is too narrow; the task needed regions not modelled\n\
               * null   → only when verdict=pass\n\
             - confidence: 0.9+ when you have direct evidence; 0.5 when uncertain."
        );

        let req = ModelRequest {
            messages: vec![
                Message::system(SYSTEM_PROMPT_REVIEWER),
                Message::user(user_prompt),
            ],
            tools: Vec::new(),
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            stop: Vec::new(),
        };
        let resp = model.complete(req).await?;
        debug!(content_len = resp.content.len(), "reviewer judge response");
        parse_verdict(&resp.content)
    }

    /// Translate a [`ReviewResult`] (when failed with Graph/Scope root_cause)
    /// into structured [`GraphError`]s for the GraphLoop to surface as
    /// `GraphInvalid { source: Review }`. Returns an empty vec when the
    /// review actually passed.
    pub fn to_graph_errors(&self, review: &ReviewResult) -> Vec<GraphError> {
        if review.passed {
            return Vec::new();
        }
        let mut out = Vec::new();
        // Start with structured info from each deterministic check that failed.
        for c in &review.deterministic_checks {
            if c.passed {
                continue;
            }
            out.push(GraphError::L0Structural {
                error_type: L0ErrorType::MissingRelation,
                detail: format!("reviewer/{}: {}", c.name, c.details),
                related_nodes: Vec::new(),
                discovered_by: Some("reviewer".into()),
            });
        }
        // Add the judge's verdict as a separate error rooted by its root_cause.
        if let Some(j) = &review.judge_verdict {
            if !j.passed {
                match j.root_cause {
                    Some(RootCause::ScopeIssue) => out.push(GraphError::ScopeGap {
                        missing_nodes: Vec::new(),
                        missing_edges: Vec::new(),
                        detail: format!("reviewer/judge: {}", j.detail),
                        discovered_by: Some("reviewer".into()),
                    }),
                    Some(RootCause::GraphIssue) | None => {
                        out.push(GraphError::L0Structural {
                            error_type: L0ErrorType::MissingRelation,
                            detail: format!("reviewer/judge: {}", j.detail),
                            related_nodes: Vec::new(),
                            discovered_by: Some("reviewer".into()),
                        });
                    }
                    Some(RootCause::TaskIssue) => {
                        // Task issues aren't graph errors — caller handles them
                        // via the embedded `judge_verdict.root_cause` directly.
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// JSON parsing
// ---------------------------------------------------------------------------

fn parse_verdict(text: &str) -> Result<JudgeVerdict> {
    let cleaned = extract_json_block(text).map_err(|e| {
        HarnessError::model(format!(
            "reviewer: judge response not parseable: {e}\n--- raw ---\n{text}"
        ))
    })?;
    let value: serde_json::Value = serde_json::from_str(&cleaned).map_err(|e| {
        HarnessError::model(format!(
            "reviewer: invalid JSON: {e}\n--- cleaned ---\n{cleaned}"
        ))
    })?;

    let verdict = value
        .get("verdict")
        .and_then(|v| v.as_str())
        .unwrap_or("fail");
    let passed = verdict == "pass";
    let root_cause = match value.get("root_cause").and_then(|v| v.as_str()) {
        Some("graph") => Some(RootCause::GraphIssue),
        Some("task") => Some(RootCause::TaskIssue),
        Some("scope") => Some(RootCause::ScopeIssue),
        _ => None,
    };
    let detail = value
        .get("detail")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let confidence = value
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5)
        .clamp(0.0, 1.0);

    Ok(JudgeVerdict {
        passed,
        root_cause,
        detail,
        confidence,
    })
}

// ---------------------------------------------------------------------------
// Render helpers — kept terse so judge prompts stay small
// ---------------------------------------------------------------------------

fn render_graph_for_judge(g: &Graph) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "nodes={} edges={} l1_entries={}\n",
        g.node_count(),
        g.edge_count(),
        g.l1.len()
    ));
    let mut ids: Vec<&NodeId> = g.nodes.keys().collect();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    if !ids.is_empty() {
        s.push_str("nodes:\n");
        for id in &ids {
            if let Some(n) = g.get_node(id) {
                s.push_str(&format!("  - {} [{:?}] {}\n", n.id, n.kind, n.summary));
            }
        }
    }
    if g.edge_count() > 0 {
        s.push_str("edges:\n");
        for e in g.iter_edges() {
            s.push_str(&format!(
                "  {} -[{:?}]-> {} (c={:.2})\n",
                e.source, e.relation, e.target, e.confidence
            ));
        }
    }
    s
}

fn render_outcome_for_judge(outcome: &DispatchOutcome) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "{} sub-task(s), all_succeeded={}, tokens={}, wall_ms={}\n",
        outcome.results.len(),
        outcome.all_succeeded,
        outcome.total_tokens,
        outcome.total_subagent_ms
    ));
    if !outcome.graph_errors.is_empty() {
        s.push_str(&format!(
            "  ⚠ {} graph error(s) reported by sub-agents\n",
            outcome.graph_errors.len()
        ));
    }
    for r in outcome.results.iter().take(8) {
        let preview = r.output.lines().next().unwrap_or("").trim();
        let truncated: String = preview.chars().take(140).collect();
        s.push_str(&format!(
            "  - {} (success={}, {} tokens): {}\n",
            r.task_id, r.success, r.tokens_used, truncated
        ));
    }
    if outcome.results.len() > 8 {
        s.push_str(&format!("  …and {} more\n", outcome.results.len() - 8));
    }
    s
}

const SYSTEM_PROMPT_REVIEWER: &str = "You are an acceptance reviewer in a graph-centric agent harness. \
You are STRICT and TERSE. Given the original task, the final relationship graph, and any sub-agent \
outcomes, you decide whether the work is genuinely done. You reply with one JSON object — no \
markdown, no prose around it. You do not propose fixes; you flag pass/fail with a root cause. \
A repairer agent or further loop iteration will handle remediation.";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::subagent::SubAgentResult;
    use crate::graph::{Edge, Graph, Node, RelationType};
    use crate::model::{FinishReason, ModelResponse, Usage};
    use async_trait::async_trait;
    use std::sync::Mutex;

    fn clean_graph() -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::file("a", "A"));
        g.add_node(Node::file("b", "B"));
        g.add_edge(Edge::new("a", "b", RelationType::Imports, 1.0, "use")).unwrap();
        g
    }

    fn cycle_graph() -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::task("t1", "T1"));
        g.add_node(Node::task("t2", "T2"));
        g.add_edge(Edge::new("t1", "t2", RelationType::DependsOn, 1.0, "")).unwrap();
        g.add_edge(Edge::new("t2", "t1", RelationType::DependsOn, 1.0, "")).unwrap();
        g
    }

    fn ok_outcome() -> DispatchOutcome {
        DispatchOutcome {
            results: vec![SubAgentResult::ok(NodeId::from("t1"), "done".into(), 100, 200)],
            batches: vec![vec![NodeId::from("t1")]],
            total_subagent_ms: 100,
            total_tokens: 200,
            all_succeeded: true,
            graph_errors: Vec::new(),
        }
    }

    fn failed_outcome() -> DispatchOutcome {
        DispatchOutcome {
            results: vec![SubAgentResult::failure(
                NodeId::from("t1"),
                "tool error".into(),
                50,
            )],
            batches: vec![vec![NodeId::from("t1")]],
            total_subagent_ms: 50,
            total_tokens: 0,
            all_succeeded: false,
            graph_errors: Vec::new(),
        }
    }

    struct MockModel {
        response: Mutex<Option<String>>,
    }
    impl MockModel {
        fn new(s: &str) -> Self {
            Self {
                response: Mutex::new(Some(s.to_string())),
            }
        }
    }
    #[async_trait]
    impl Model for MockModel {
        fn name(&self) -> &str {
            "mock-reviewer"
        }
        async fn complete(&self, _: ModelRequest) -> Result<ModelResponse> {
            let content = self
                .response
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| "{}".to_string());
            Ok(ModelResponse {
                content,
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                reasoning_content: None,
                usage: Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn deterministic_only_passes_clean_graph_and_ok_outcome() {
        let r = Reviewer::deterministic_only();
        let result = r.review("task", &clean_graph(), Some(&ok_outcome()), None).await.unwrap();
        assert!(result.passed);
        assert!(result.judge_verdict.is_none());
        assert!(result.deterministic_checks.iter().all(|c| c.passed));
    }

    #[tokio::test]
    async fn deterministic_only_fails_cycle_in_graph() {
        let r = Reviewer::deterministic_only();
        let result = r.review("task", &cycle_graph(), Some(&ok_outcome()), None).await.unwrap();
        assert!(!result.passed);
        let cons = result
            .deterministic_checks
            .iter()
            .find(|c| c.name == "graph_consistency")
            .expect("graph_consistency check present");
        assert!(!cons.passed);
    }

    #[tokio::test]
    async fn deterministic_only_fails_when_subagent_fails() {
        let r = Reviewer::deterministic_only();
        let result = r.review("task", &clean_graph(), Some(&failed_outcome()), None).await.unwrap();
        assert!(!result.passed);
        let sub = result
            .deterministic_checks
            .iter()
            .find(|c| c.name == "subagent_results")
            .expect("subagent_results check present");
        assert!(!sub.passed);
    }

    #[tokio::test]
    async fn deterministic_only_flags_subagent_graph_errors() {
        let mut outcome = ok_outcome();
        outcome.graph_errors.push(GraphError::L0Structural {
            error_type: L0ErrorType::MissingRelation,
            detail: "missing call edge".into(),
            related_nodes: vec![NodeId::from("a")],
            discovered_by: Some("t1".into()),
        });
        outcome.all_succeeded = false;
        let r = Reviewer::deterministic_only();
        let result = r.review("task", &clean_graph(), Some(&outcome), None).await.unwrap();
        assert!(!result.passed);
        assert!(
            result
                .deterministic_checks
                .iter()
                .any(|c| c.name == "subagent_graph_errors" && !c.passed)
        );
    }

    #[tokio::test]
    async fn judge_pass_combined_with_deterministic_pass_yields_pass() {
        let resp = r#"{"verdict":"pass","detail":"covers task","confidence":0.9}"#;
        let r = Reviewer::with_model(Arc::new(MockModel::new(resp)));
        let result = r.review("task", &clean_graph(), Some(&ok_outcome()), None).await.unwrap();
        assert!(result.passed);
        let j = result.judge_verdict.unwrap();
        assert!(j.passed);
        assert!(j.root_cause.is_none());
        assert!((j.confidence - 0.9).abs() < 1e-6);
    }

    #[tokio::test]
    async fn judge_fail_with_graph_root_cause_routes_to_graph_invalid() {
        let resp = r#"{"verdict":"fail","root_cause":"graph","detail":"missing auth module link","confidence":0.85}"#;
        let r = Reviewer::with_model(Arc::new(MockModel::new(resp)));
        let result = r.review("task", &clean_graph(), Some(&ok_outcome()), None).await.unwrap();
        assert!(!result.passed);
        let j = result.judge_verdict.as_ref().unwrap();
        assert!(!j.passed);
        assert_eq!(j.root_cause, Some(RootCause::GraphIssue));
        // to_graph_errors should produce at least one L0Structural error
        let errs = r.to_graph_errors(&result);
        assert!(!errs.is_empty());
        assert!(errs.iter().any(|e| matches!(e, GraphError::L0Structural { .. })));
    }

    #[tokio::test]
    async fn judge_fail_with_scope_root_cause_produces_scope_gap_error() {
        let resp = r#"{"verdict":"fail","root_cause":"scope","detail":"task needs a settings module","confidence":0.7}"#;
        let r = Reviewer::with_model(Arc::new(MockModel::new(resp)));
        let result = r.review("task", &clean_graph(), Some(&ok_outcome()), None).await.unwrap();
        let errs = r.to_graph_errors(&result);
        assert!(errs.iter().any(|e| matches!(e, GraphError::ScopeGap { .. })));
    }

    #[tokio::test]
    async fn judge_fail_with_task_root_cause_does_not_produce_graph_errors() {
        let resp = r#"{"verdict":"fail","root_cause":"task","detail":"sub-agents missed coverage","confidence":0.8}"#;
        let r = Reviewer::with_model(Arc::new(MockModel::new(resp)));
        let result = r.review("task", &clean_graph(), Some(&ok_outcome()), None).await.unwrap();
        assert!(!result.passed);
        let errs = r.to_graph_errors(&result);
        // Task-issue should NOT produce graph errors (GraphLoop will route differently)
        assert!(errs.is_empty(), "task root_cause should yield no graph errors, got: {errs:?}");
    }

    #[tokio::test]
    async fn judge_markdown_fence_handled() {
        let resp = "```json\n{\"verdict\":\"pass\",\"detail\":\"ok\",\"confidence\":0.8}\n```";
        let r = Reviewer::with_model(Arc::new(MockModel::new(resp)));
        let result = r.review("task", &clean_graph(), Some(&ok_outcome()), None).await.unwrap();
        assert!(result.passed);
    }

    #[tokio::test]
    async fn deterministic_fail_overrides_judge_pass() {
        // Cycle in graph + judge says pass → review still fails (deterministic
        // is hard gate).
        let resp = r#"{"verdict":"pass","detail":"looks fine to me","confidence":0.9}"#;
        let r = Reviewer::with_model(Arc::new(MockModel::new(resp)));
        let result = r.review("task", &cycle_graph(), Some(&ok_outcome()), None).await.unwrap();
        assert!(!result.passed);
    }

    #[test]
    fn root_cause_accessor() {
        let result = ReviewResult {
            passed: false,
            deterministic_checks: Vec::new(),
            judge_verdict: Some(JudgeVerdict {
                passed: false,
                root_cause: Some(RootCause::ScopeIssue),
                detail: "x".into(),
                confidence: 0.7,
            }),
            rationale: "x".into(),
        };
        assert_eq!(result.root_cause(), Some(&RootCause::ScopeIssue));
    }

    #[test]
    fn no_judge_when_no_model() {
        // Just confirms deterministic_only() produces None for model.
        let r = Reviewer::deterministic_only();
        assert!(r.model.is_none());
    }
}
