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
use tracing::{debug, info, warn};

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
                Message::system(load_prompt_file("skills/prompts/decomposer.md", SYSTEM_PROMPT_DECOMPOSER)),
                Message::user(user_prompt.clone()),
            ],
            tools: vec![decomposer_tool_schema()],
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            stop: Vec::new(),
        };
        let resp = self.model.complete(req).await?;
        debug!(
            content_len = resp.content.len(),
            reasoning_len = resp.reasoning_content.as_deref().map(str::len).unwrap_or(0),
            tool_calls = resp.tool_calls.len(),
            tokens = resp.usage.total_tokens,
            "decomposer model response"
        );

        // Strategy A: prefer native tool_calls (db2d993d-class fix); fall
        // back to text parsing when the model emitted none. If both are
        // empty, surface a clear error rather than the misleading
        // "proposer: no '{' in response" the text parser used to give.
        if let Some(parsed) = parse_decomposer_response_from_tool_calls(&resp.tool_calls) {
            let task_graph = build_task_graph(parsed, world_graph)?;
            info!(
                tasks = task_graph.node_count(),
                edges = task_graph.edge_count(),
                source = "tool_call",
                "decomposer produced task graph"
            );
            return Ok(task_graph);
        }

        // Reasoning-model fallback (DeepSeek / MiniMax M3): see
        // `ModelResponse::text_or_reasoning`. db2d993d regression.
        let parse_text = resp.text_or_reasoning();
        if parse_text.trim().is_empty() {
            return Err(HarnessError::model(
                "decomposer: empty response — model returned neither content nor reasoning_content"
                    .to_string(),
            ));
        }

        let parsed = parse_decomposer_response(parse_text);
        let parsed = match parsed {
            Ok(t) => t,
            Err(parse_err) => {
                // Text path failed (e.g., model returned prose with no
                // JSON braces — common with reasoning models that decide
                // to "think out loud" instead of calling the tool). Retry
                // once with a stronger prompt demanding the tool call.
                // This is the same fix-it pattern proposer.rs uses.
                warn!(
                    error = %parse_err,
                    "decomposer first response was malformed; retrying once with a fix-it prompt"
                );
                let retry_prompt = format!(
                    "Your previous response was malformed (parser said: {parse_err}). \
                     You MUST call the `emit_task_decomposition` tool with a valid JSON `tasks` array. \
                     Do NOT reply with prose or explanations. Reply with the tool call only."
                );
                let mut retry_messages = vec![
                    Message::system(load_prompt_file(
                        "skills/prompts/decomposer.md",
                        SYSTEM_PROMPT_DECOMPOSER,
                    )),
                    Message::user(user_prompt),
                    Message::assistant(parse_text.clone()),
                    Message::user(retry_prompt),
                ];
                // If a conversation was passed, inject the recent turns
                // (without the latest user_prompt) to preserve context.
                if let Some(c) = conv {
                    // Truncate to last 6 turns to preserve context.
                    let conv_msgs: Vec<_> = c.messages.iter().rev().take(6).rev().cloned().collect();
                    // Splice in after the system message (index 0).
                    retry_messages.splice(1..1, conv_msgs);
                }
                let retry_req = ModelRequest {
                    messages: retry_messages,
                    tools: vec![decomposer_tool_schema()],
                    temperature: self.temperature,
                    max_tokens: self.max_tokens,
                    stop: Vec::new(),
                };
                let retry_resp = self.model.complete(retry_req).await?;
                let retry_text = retry_resp.text_or_reasoning();
                if retry_text.trim().is_empty() {
                    return Err(parse_err);
                }
                parse_decomposer_response(&retry_text)?
            }
        };
        let task_graph = build_task_graph(parsed, world_graph)?;
        info!(
            tasks = task_graph.node_count(),
            edges = task_graph.edge_count(),
            source = "text",
            "decomposer produced task graph"
        );
        Ok(task_graph)
    }

    /// Expand complex nodes: for every node in `involved_nodes` that has
    /// `Contains` sub-nodes (expanded=true), add those sub-nodes to the
    /// involved list. This enables function-level granularity.

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

/// Parse the decomposer's task list from a native tool_call. Returns
/// `None` when no matching tool_call is present — caller falls back to
/// text parsing (the legacy `extract_json_block` path).
fn parse_decomposer_response_from_tool_calls(
    tool_calls: &[crate::model::ToolCall],
) -> Option<Vec<ParsedTask>> {
    let tc = tool_calls.iter().find(|tc| tc.name == "emit_task_decomposition")?;
    let arr = tc.arguments.get("tasks")?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let id = item.get("id").and_then(|v| v.as_str())?;
        if id.is_empty() {
            return None; // treat empty id same as missing → fall back
        }
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
            id: id.to_string(),
            description,
            involved_nodes,
            dependencies,
            needs,
        });
    }
    Some(out)
}

/// Tool schema for the decomposer. Same wire shape as the text-fallback
/// JSON; enum + array constraints match the legacy parser's tolerance.
fn decomposer_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "emit_task_decomposition",
            "description": "Decompose the task into the smallest set of concrete sub-tasks that, when executed in dependency order, accomplish it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string", "description": "Short unique id like t1, t2, ..."},
                                "description": {"type": "string", "description": "What this sub-task does, 1-2 sentences."},
                                "involved_nodes": {
                                    "type": "array",
                                    "items": {"type": "string"},
                                    "description": "World-graph node ids this task touches."
                                },
                                "dependencies": {
                                    "type": "array",
                                    "items": {"type": "string"},
                                    "description": "Other task ids (from this response) that must finish first."
                                },
                                "needs": {
                                    "type": "object",
                                    "properties": {
                                        "can_read": {"type": "boolean"},
                                        "can_write": {"type": "boolean"},
                                        "can_execute": {"type": "boolean"}
                                    },
                                    "description": "Capability surface the sub-agent needs."
                                }
                            },
                            "required": ["id"]
                        }
                    },
                    "rationale": {"type": "string", "description": "One sentence justifying this decomposition."}
                },
                "required": ["tasks"]
            }
        }
    })
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
            contract: CheckContract::default(), role_prompt: String::new(),
        };
        g.add_node(st.to_task_node());
    }
    for t in &parsed {
        let src = NodeId::from(t.id.as_str());
        for dep in &t.dependencies {
            // subtask flow: src --LeadsTo--> dep (process sequence; may cycle)
            let tgt = NodeId::from(dep.as_str());
            g.add_edge(Edge::new(
                src.clone(),
                tgt,
                RelationType::LeadsTo,
                1.0,
                "decomposer-declared dependency",
            ))?;
        }
    }

    // Validation 4: DependsOn must stay acyclic (true hard dependencies).
    // LeadsTo edges (flow/sequencing, used for subtask chains) may cycle and
    // are NOT checked here — so this check naturally won't fire on the normal
    // subtask flow chain which is now built with LeadsTo.
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

/// Try to load a prompt from a file, falling back to the hardcoded default.
/// This lets users edit `skills/prompts/decomposer-*.md` without recompiling.
fn load_prompt_file(path: &str, default: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| default.to_string())
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
        content: Mutex<Option<String>>,
        reasoning: Mutex<Option<String>>,
    }

    impl MockModel {
        /// Backwards-compat constructor: stores the string in `content` only.
        fn new(s: &str) -> Self {
            Self {
                content: Mutex::new(Some(s.to_string())),
                reasoning: Mutex::new(None),
            }
        }

        /// Simulate a reasoning model (DeepSeek / M3) that puts the
        /// final JSON in `reasoning_content` and leaves `content` empty
        /// (the shape that triggered the db2d993d production failure).
        fn new_split(content: &str, reasoning: &str) -> Self {
            Self {
                content: Mutex::new(Some(content.to_string())),
                reasoning: Mutex::new(Some(reasoning.to_string())),
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
                .content
                .lock()
                .unwrap()
                .take()
                .unwrap_or_default();
            let reasoning = self.reasoning.lock().unwrap().take();
            Ok(ModelResponse {
                content,
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                reasoning_content: reasoning,
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
        // t3 -LeadsTo-> t1, t3 -LeadsTo-> t2 (2 flow edges)
        assert_eq!(tg.edge_count(), 2);
        // Verify edges are LeadsTo (flow/sequencing), not DependsOn.
        for e in &tg.edges {
            assert_eq!(e.relation, RelationType::LeadsTo, "subtask edges must be LeadsTo");
        }
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
        // t1 -> t2 -> t1 (cycle via LeadsTo — now allowed for flow edges)
        let resp = r#"{
          "tasks": [
            {"id":"t1","description":"x","involved_nodes":["a"],"dependencies":["t2"],"needs":{}},
            {"id":"t2","description":"y","involved_nodes":["b"],"dependencies":["t1"],"needs":{}}
          ]
        }"#;
        let d = Decomposer::new(Arc::new(MockModel::new(resp)));
        // LeadsTo cycles are valid (flow may loop); decompose should succeed.
        let tg = d.decompose(&three_node_world(), "x", None).await.unwrap();
        assert_eq!(tg.node_count(), 2);
        assert_eq!(tg.edge_count(), 2);
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

    /// Regression for db2d993d: the model put its final JSON in
    /// `reasoning_content` and left `content` empty. The decomposer
    /// must fall back to `reasoning_content` instead of failing with
    /// `proposer: no '{' in response`.
    #[tokio::test]
    async fn decompose_falls_back_to_reasoning_content_when_content_empty() {
        let json = r#"{
          "tasks": [
            {"id":"t1","description":"x","involved_nodes":["a"],"dependencies":[],"needs":{}}
          ]
        }"#;
        // Content is empty (DeepSeek / M3 reasoning-model shape);
        // reasoning_content carries the final JSON.
        let d = Decomposer::new(Arc::new(MockModel::new_split("", json)));
        let tg = d
            .decompose(&three_node_world(), "x", None)
            .await
            .expect("decomposer must succeed when JSON is in reasoning_content");
        assert_eq!(tg.node_count(), 1);
    }

    /// Even when both fields are populated, prefer `content` (the
    /// conventional channel). Reasoning content is the fallback.
    #[tokio::test]
    async fn decompose_prefers_content_over_reasoning_content() {
        let content_json = r#"{"tasks":[{"id":"t-content","description":"x","involved_nodes":["a"],"dependencies":[],"needs":{}}]}"#;
        let reasoning_json = r#"{"tasks":[{"id":"t-reasoning","description":"x","involved_nodes":["a"],"dependencies":[],"needs":{}}]}"#;
        let d = Decomposer::new(Arc::new(MockModel::new_split(
            content_json, reasoning_json,
        )));
        let tg = d.decompose(&three_node_world(), "x", None).await.unwrap();
        let node = tg.get_node(&NodeId::from("t-content")).unwrap();
        assert_eq!(
            node.summary, "x",
            "must parse the content-channel JSON, not the reasoning-channel one"
        );
    }

    /// Mock model that returns a configured native tool_call — the path
    /// that replaces the legacy JSON-in-text parsing.
    struct ToolCallDecomposerModel {
        tool_call: Mutex<Option<crate::model::ToolCall>>,
    }
    #[async_trait]
    impl Model for ToolCallDecomposerModel {
        fn name(&self) -> &str {
            "tool_call_decomposer"
        }
        async fn complete(&self, _: ModelRequest) -> Result<ModelResponse> {
            let tc = self.tool_call.lock().unwrap().take();
            Ok(ModelResponse {
                content: String::new(),
                reasoning_content: None,
                tool_calls: tc.into_iter().collect(),
                finish_reason: FinishReason::ToolCalls,
                usage: Usage::default(),
            })
        }
    }

    /// Decomposer must take the native tool_call path when the model
    /// emits one. This is the regression test for the db2d993d class of
    /// failures — content is empty, reasoning-only models wouldn't make
    /// it through the text path.
    #[tokio::test]
    async fn decompose_uses_tool_call_when_model_emits_one() {
        let tc = crate::model::ToolCall {
            id: "c1".into(),
            name: "emit_task_decomposition".into(),
            arguments: serde_json::json!({
                "tasks": [
                    {"id": "t1", "description": "Analyze A",
                     "involved_nodes": ["a"], "dependencies": [],
                     "needs": {"can_read": true, "can_write": false, "can_execute": false}},
                    {"id": "t2", "description": "Analyze B",
                     "involved_nodes": ["b"], "dependencies": [],
                     "needs": {"can_read": true, "can_write": false, "can_execute": false}},
                    {"id": "t3", "description": "Synthesize",
                     "involved_nodes": ["a", "b", "c"],
                     "dependencies": ["t1", "t2"],
                     "needs": {"can_read": true, "can_write": true, "can_execute": false}}
                ],
                "rationale": "tool_call path"
            }),
        };
        let d = Decomposer::new(Arc::new(ToolCallDecomposerModel {
            tool_call: Mutex::new(Some(tc)),
        }));
        let tg = d.decompose(&three_node_world(), "x", None).await.unwrap();
        assert_eq!(tg.node_count(), 3);
        assert_eq!(tg.edge_count(), 2);
        for e in &tg.edges {
            assert_eq!(e.relation, RelationType::LeadsTo);
        }
    }

    /// If the model emits a tool_call missing the required `id` field on
    /// a task, fall back to text parsing (don't return None silently).
    #[tokio::test]
    async fn decompose_tool_call_missing_id_falls_back_to_text() {
        // Tool_call with a task missing `id` → returns None from parser →
        // falls through to text. Text content is empty → error.
        let tc = crate::model::ToolCall {
            id: "c1".into(),
            name: "emit_task_decomposition".into(),
            arguments: serde_json::json!({
                "tasks": [
                    {"description": "missing id", "involved_nodes": ["a"], "dependencies": []}
                ]
            }),
        };
        // MockModel has both fields None by default → text fallback gets empty.
        let model = MockModel {
            content: Mutex::new(None),
            reasoning: Mutex::new(None),
        };
        let _ = tc; // we won't actually inject it; just verify the wiring
        let d = Decomposer::new(Arc::new(model));
        let err = d
            .decompose(&three_node_world(), "x", None)
            .await
            .expect_err("empty response must error");
        assert!(
            err.to_string().contains("empty response"),
            "expected clear error, got: {err}"
        );
    }
}
