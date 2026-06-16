//! Decomposer — model turns a verified world graph into a task DAG.
//!
//! Phase 3 entry point. Once the Graph phase declares the relationship
//! graph "verified", the Decomposer is invoked to ask the model: *given
//! this graph and this task, what concrete sub-tasks should the harness
//! dispatch?*
//!
//! The output is a [`Graph`] of `NodeKind::Task` nodes connected by
//! `RelationType::DependsOn` edges. This shape is deliberate: it's the
//! same `Graph` type used everywhere else, so the existing `DagScheduler`
//! handles topological scheduling without any new code.
//!
//! ## Output wire schema
//!
//! ```json
//! {
//!   "tasks": [
//!     {
//!       "id": "t1",
//!       "description": "Analyze module A and report on its responsibility",
//!       "involved_nodes": ["a"],
//!       "dependencies": [],
//!       "needs": {"can_read": true, "can_write": false, "can_execute": false}
//!     },
//!     {
//!       "id": "t2",
//!       "description": "Cross-reference findings from t1 with module B",
//!       "involved_nodes": ["a", "b"],
//!       "dependencies": ["t1"],
//!       "needs": {"can_read": true, "can_write": false, "can_execute": false}
//!     }
//!   ],
//!   "rationale": "<why this decomposition>"
//! }
//! ```
//!
//! ## Validation
//!
//! - All task ids are unique.
//! - Every `involved_nodes` entry exists in the world graph.
//! - Every `dependencies` entry references another task in the same response.
//! - No cycles in the resulting task DAG (caught later by `DagScheduler`,
//!   but we also check here to fail fast with a clearer error).

use super::contract::CheckContract;
use super::proposer::extract_json_block;
use super::subagent::SubTask;
use super::Conversation;
use crate::domain::TaskNeeds;
use crate::error::{HarnessError, Result};
use crate::graph::{Edge, Graph, NodeId, RelationType};
use crate::model::{Message, Model, ModelRequest, Role};
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, info};

#[derive(Clone)]
pub struct Decomposer {
    pub model: Arc<dyn Model>,
    pub temperature: f64,
    pub max_tokens: Option<usize>,
}

impl Decomposer {
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self {
            model,
            temperature: 0.2,
            // Decomposition can produce many tasks; generous cap.
            max_tokens: Some(8192),
        }
    }

    pub fn with_temperature(mut self, t: f64) -> Self {
        self.temperature = t;
        self
    }

    pub fn with_max_tokens(mut self, n: usize) -> Self {
        self.max_tokens = Some(n);
        self
    }

    /// Ask the model to decompose `task` over the structure of `world_graph`.
    /// Returns a [`Graph`] of task nodes ready to feed into a `DagScheduler`.
    ///
    /// `conv` is optional context — when present, the last few user/assistant
    /// turns are appended to the prompt so the decomposer benefits from the
    /// clarifications that happened during graph building.
    pub async fn decompose(
        &self,
        world_graph: &Graph,
        task: &str,
        conv: Option<&Conversation>,
    ) -> Result<Graph> {
        let graph_sketch = render_world_for_decomposer(world_graph);
        let recent_conv = conv.map(|c| recent_turns(c, 6)).unwrap_or_default();

        let user_prompt = format!(
            "## Task\n{task}\n\n## World graph (verified)\n{graph_sketch}\n\n\
             ## Recent clarifying dialog\n{recent_conv}\n\n\
             Decompose the task into the smallest set of concrete sub-tasks that, when executed in \
             dependency order, accomplish the task. Reply with ONE JSON object only (no markdown):\n\n\
             {{\n  \"tasks\": [\n    {{\n      \"id\": \"<short unique id like t1, t2, ...>\",\n      \
             \"description\": \"<what this sub-task does, 1-2 sentences>\",\n      \
             \"involved_nodes\": [\"<world_graph_node_id>\", ...],\n      \
             \"dependencies\": [\"<other_task_id>\", ...],\n      \
             \"needs\": {{\"can_read\": bool, \"can_write\": bool, \"can_execute\": bool}}\n    }}\n  ],\n  \
             \"rationale\": \"<one sentence justifying this decomposition>\"\n}}\n\n\
             Rules:\n\
             - Task ids must be unique within this response.\n\
             - `involved_nodes` entries MUST appear in the world graph above; the runtime rejects \
               anything else.\n\
             - `dependencies` are other task ids from THIS response. No forward refs to tasks you \
               haven't listed yet.\n\
             - `needs` declares the capability surface a sub-agent needs: `can_read` covers \
               reading source/data, `can_write` covers mutation, `can_execute` covers running \
               shell/code. Be conservative — flip `can_write` on only when a task genuinely needs \
               to change something.\n\
             - Prefer many small tasks over few large ones. If a task touches more than ~5 nodes \
               or has more than ~3 dependencies, split it."
        );

        let req = ModelRequest {
            messages: vec![
                Message::system(SYSTEM_PROMPT_DECOMPOSER),
                Message::user(user_prompt),
            ],
            tools: Vec::new(),
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            stop: Vec::new(),
        };
        let resp = self.model.complete(req).await?;
        debug!(
            content_len = resp.content.len(),
            tokens = resp.usage.total_tokens,
            "decomposer model response"
        );

        let parsed = parse_decomposer_response(&resp.content)?;
        let task_graph = build_task_graph(parsed, world_graph)?;
        info!(
            tasks = task_graph.node_count(),
            edges = task_graph.edge_count(),
            "decomposer produced task graph"
        );
        Ok(task_graph)
    }

    /// Expand complex nodes: for every node in `involved_nodes` that has
    /// `Contains` sub-nodes (expanded=true), add those sub-nodes to the
    /// involved list. This enables function-level granularity.
    pub fn expand_involved_nodes(world_graph: &Graph, involved: &[NodeId]) -> Vec<NodeId> {
        let mut expanded = involved.to_vec();
        for id in involved {
            // Collect child nodes via Contains edges.
            let children: Vec<NodeId> = world_graph
                .outgoing(id)
                .filter(|e| e.relation == RelationType::Contains)
                .map(|e| e.target.clone())
                .collect();
            for child in &children {
                if !expanded.contains(child) {
                    expanded.push(child.clone());
                }
            }
        }
        expanded
    }
}

// ---------------------------------------------------------------------------
// Parse
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ParsedTask {
    id: String,
    description: String,
    involved_nodes: Vec<NodeId>,
    dependencies: Vec<String>,
    needs: TaskNeeds,
}

fn parse_decomposer_response(text: &str) -> Result<Vec<ParsedTask>> {
    let cleaned = extract_json_block(text)?;
    let value: serde_json::Value = serde_json::from_str(&cleaned).map_err(|e| {
        HarnessError::model(format!(
            "decomposer: invalid JSON: {e}\n--- raw ---\n{text}\n--- cleaned ---\n{cleaned}"
        ))
    })?;

    let tasks_arr = value
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| HarnessError::model("decomposer: response missing 'tasks' array"))?;

    let mut out = Vec::with_capacity(tasks_arr.len());
    for (i, item) in tasks_arr.iter().enumerate() {
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                HarnessError::model(format!("decomposer: task[{i}] missing 'id' string"))
            })?
            .to_string();
        let description = item
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let involved_nodes: Vec<NodeId> = item
            .get("involved_nodes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(NodeId::from))
                    .collect()
            })
            .unwrap_or_default();
        let dependencies: Vec<String> = item
            .get("dependencies")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let needs: TaskNeeds = item
            .get("needs")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        out.push(ParsedTask {
            id,
            description,
            involved_nodes,
            dependencies,
            needs,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Build task graph + validate
// ---------------------------------------------------------------------------

fn build_task_graph(parsed: Vec<ParsedTask>, world_graph: &Graph) -> Result<Graph> {
    // Validation 1: task ids unique.
    let mut seen_ids: HashSet<String> = HashSet::new();
    for t in &parsed {
        if !seen_ids.insert(t.id.clone()) {
            return Err(HarnessError::model(format!(
                "decomposer: duplicate task id `{}`",
                t.id
            )));
        }
    }

    // Validation 2: involved_nodes exist in world graph.
    for t in &parsed {
        for n in &t.involved_nodes {
            if !world_graph.contains_node(n) {
                return Err(HarnessError::model(format!(
                    "decomposer: task `{}` references world-graph node `{}` that does not exist",
                    t.id, n
                )));
            }
        }
    }

    // Validation 3: dependencies reference declared tasks (no forward refs
    // are technically allowed, but every dep must be in `seen_ids`).
    for t in &parsed {
        for dep in &t.dependencies {
            if !seen_ids.contains(dep) {
                return Err(HarnessError::model(format!(
                    "decomposer: task `{}` depends on `{}` which is not in the task list",
                    t.id, dep
                )));
            }
            if dep == &t.id {
                return Err(HarnessError::model(format!(
                    "decomposer: task `{}` depends on itself",
                    t.id
                )));
            }
        }
    }

    // Build the graph. SubTask::to_task_node handles the metadata round-trip.
    let mut g = Graph::new();
    for t in &parsed {
        let st = SubTask {
            id: NodeId::from(t.id.as_str()),
            description: t.description.clone(),
            involved_nodes: t.involved_nodes.clone(),
            needs: t.needs.clone(),
            contract: CheckContract::default(),
        };
        g.add_node(st.to_task_node());
    }
    for t in &parsed {
        let src = NodeId::from(t.id.as_str());
        for dep in &t.dependencies {
            // dependent -DependsOn-> dependency (matches scheduler convention)
            let tgt = NodeId::from(dep.as_str());
            g.add_edge(Edge::new(
                src.clone(),
                tgt,
                RelationType::DependsOn,
                1.0,
                "decomposer-declared dependency",
            ))?;
        }
    }

    // Validation 4: catch cycles early (the scheduler would catch this too,
    // but a clearer error here saves the loop a round-trip).
    if let Some(cycle) = g.find_cycle_in_relation(RelationType::DependsOn) {
        return Err(HarnessError::model(format!(
            "decomposer: cycle in task DAG via DependsOn: {:?}",
            cycle
        )));
    }

    Ok(g)
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

fn render_world_for_decomposer(g: &Graph) -> String {
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
                let l1_hint = g
                    .l1
                    .get(id)
                    .filter(|d| !d.is_blank())
                    .map(|d| format!(" | L1: {}", d.render_oneline()))
                    .unwrap_or_default();
                s.push_str(&format!(
                    "  - {} [{:?}] {}{l1_hint}\n",
                    n.id, n.kind, n.summary
                ));
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

fn recent_turns(conv: &Conversation, max_turns: usize) -> String {
    let mut out = String::new();
    let total = conv.messages.len();
    let start = total.saturating_sub(max_turns);
    for m in &conv.messages[start..] {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "agent",
            Role::Tool => "tool",
            Role::System => continue,
        };
        out.push_str(&format!("[{role}] {}\n", m.content));
    }
    if out.is_empty() {
        out.push_str("(no prior dialog)\n");
    }
    out
}

const SYSTEM_PROMPT_DECOMPOSER: &str = "You are a decomposer in a graph-centric agent harness. \
You are handed a verified relationship graph and a task description; your single job is to break \
the task into a small DAG of concrete sub-tasks that downstream sub-agents will execute. You output \
exactly one JSON object — no markdown, no prose. You prefer many small focused tasks over few \
large vague ones. You declare dependencies and capability needs honestly: a task that only reads \
data must not claim it needs write or execute capability.";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Node;
    use crate::model::{FinishReason, ModelResponse, Usage};
    use async_trait::async_trait;
    use std::sync::Mutex;

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
            "mock-decomposer"
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
                usage: Usage::default(),
            })
        }
    }

    fn three_node_world() -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::file("a", "module A"));
        g.add_node(Node::file("b", "module B"));
        g.add_node(Node::file("c", "module C"));
        g
    }

    #[tokio::test]
    async fn decompose_builds_task_dag_with_correct_dependencies() {
        let resp = r#"{
          "tasks": [
            {"id":"t1","description":"Analyze A","involved_nodes":["a"],"dependencies":[],"needs":{"can_read":true,"can_write":false,"can_execute":false}},
            {"id":"t2","description":"Analyze B","involved_nodes":["b"],"dependencies":[],"needs":{"can_read":true,"can_write":false,"can_execute":false}},
            {"id":"t3","description":"Synthesize from t1,t2","involved_nodes":["a","b","c"],"dependencies":["t1","t2"],"needs":{"can_read":true,"can_write":true,"can_execute":false}}
          ],
          "rationale":"two analyses then a synthesis"
        }"#;
        let d = Decomposer::new(Arc::new(MockModel::new(resp)));
        let tg = d.decompose(&three_node_world(), "produce a report", None).await.unwrap();
        assert_eq!(tg.node_count(), 3);
        // t3 -DependsOn-> t1, t3 -DependsOn-> t2 (2 edges)
        assert_eq!(tg.edge_count(), 2);
        // Check that DagScheduler can handle this output.
        let s = crate::scheduler::DagScheduler::new().plan(&tg).unwrap();
        assert_eq!(s.depth(), 2); // [t1,t2], [t3]
        assert_eq!(s.task_count(), 3);
    }

    #[tokio::test]
    async fn decompose_rejects_involved_node_not_in_world_graph() {
        let resp = r#"{
          "tasks": [
            {"id":"t1","description":"x","involved_nodes":["ghost"],"dependencies":[],"needs":{}}
          ]
        }"#;
        let d = Decomposer::new(Arc::new(MockModel::new(resp)));
        let err = d.decompose(&three_node_world(), "x", None).await.unwrap_err();
        assert!(format!("{err}").contains("does not exist"));
    }

    #[tokio::test]
    async fn decompose_rejects_dependency_on_undeclared_task() {
        let resp = r#"{
          "tasks": [
            {"id":"t1","description":"x","involved_nodes":["a"],"dependencies":["t99"],"needs":{}}
          ]
        }"#;
        let d = Decomposer::new(Arc::new(MockModel::new(resp)));
        let err = d.decompose(&three_node_world(), "x", None).await.unwrap_err();
        assert!(format!("{err}").contains("not in the task list"));
    }

    #[tokio::test]
    async fn decompose_rejects_self_dependency() {
        let resp = r#"{
          "tasks": [
            {"id":"t1","description":"x","involved_nodes":["a"],"dependencies":["t1"],"needs":{}}
          ]
        }"#;
        let d = Decomposer::new(Arc::new(MockModel::new(resp)));
        let err = d.decompose(&three_node_world(), "x", None).await.unwrap_err();
        assert!(format!("{err}").contains("depends on itself"));
    }

    #[tokio::test]
    async fn decompose_rejects_duplicate_task_ids() {
        let resp = r#"{
          "tasks": [
            {"id":"t1","description":"x","involved_nodes":["a"],"dependencies":[],"needs":{}},
            {"id":"t1","description":"y","involved_nodes":["b"],"dependencies":[],"needs":{}}
          ]
        }"#;
        let d = Decomposer::new(Arc::new(MockModel::new(resp)));
        let err = d.decompose(&three_node_world(), "x", None).await.unwrap_err();
        assert!(format!("{err}").contains("duplicate task id"));
    }

    #[tokio::test]
    async fn decompose_detects_cycle_in_task_dag() {
        // t1 -> t2 -> t1 (cycle)
        let resp = r#"{
          "tasks": [
            {"id":"t1","description":"x","involved_nodes":["a"],"dependencies":["t2"],"needs":{}},
            {"id":"t2","description":"y","involved_nodes":["b"],"dependencies":["t1"],"needs":{}}
          ]
        }"#;
        let d = Decomposer::new(Arc::new(MockModel::new(resp)));
        let err = d.decompose(&three_node_world(), "x", None).await.unwrap_err();
        assert!(format!("{err}").contains("cycle"));
    }

    #[tokio::test]
    async fn decompose_empty_task_list_produces_empty_graph() {
        let resp = r#"{"tasks": [], "rationale": "task is trivial"}"#;
        let d = Decomposer::new(Arc::new(MockModel::new(resp)));
        let tg = d.decompose(&three_node_world(), "trivial", None).await.unwrap();
        assert_eq!(tg.node_count(), 0);
    }

    #[tokio::test]
    async fn decompose_missing_tasks_field_errors() {
        let resp = r#"{"rationale": "no tasks key"}"#;
        let d = Decomposer::new(Arc::new(MockModel::new(resp)));
        let err = d.decompose(&three_node_world(), "x", None).await.unwrap_err();
        assert!(format!("{err}").contains("'tasks'"));
    }

    #[tokio::test]
    async fn decompose_handles_markdown_fence_wrapping() {
        let resp = "```json\n{\"tasks\":[{\"id\":\"t1\",\"description\":\"x\",\"involved_nodes\":[\"a\"],\"dependencies\":[],\"needs\":{}}]}\n```";
        let d = Decomposer::new(Arc::new(MockModel::new(resp)));
        let tg = d.decompose(&three_node_world(), "x", None).await.unwrap();
        assert_eq!(tg.node_count(), 1);
    }

    #[tokio::test]
    async fn decomposed_task_nodes_preserve_involved_nodes_and_needs() {
        let resp = r#"{
          "tasks": [
            {"id":"t1","description":"write to A","involved_nodes":["a","b"],"dependencies":[],"needs":{"can_read":true,"can_write":true,"can_execute":false}}
          ]
        }"#;
        let d = Decomposer::new(Arc::new(MockModel::new(resp)));
        let tg = d.decompose(&three_node_world(), "x", None).await.unwrap();
        let node = tg.get_node(&NodeId::from("t1")).unwrap();
        let st = SubTask::from_task_node(node).unwrap();
        assert_eq!(st.involved_nodes, vec![NodeId::from("a"), NodeId::from("b")]);
        assert!(st.needs.can_read);
        assert!(st.needs.can_write);
        assert!(!st.needs.can_execute);
    }
}
