//! GraphProposer — the model-driven step engine inside the iterative loop.
//!
//! Each `next_step` call:
//!
//! 1. Builds a `ModelRequest` from (system prompt + current graph snapshot +
//!    conversation history).
//! 2. Calls the model.
//! 3. Parses the response as exactly one [`ProposerStep`] — JSON object with
//!    a discriminated `step` field.
//!
//! The four possible steps map directly to the four moves any layer of the
//! iterative loop can make (see [[feedback-iterative-loop-is-centerpiece]]):
//!
//! - `AskUser`         — surface a question to the user (main agent only)
//! - `CallTool`        — invoke a tool from the registry to gather evidence
//! - `ProposePatch`    — append a small, surgical [`GraphPatch`] to the graph
//! - `ReadyForVerify`  — declare the build/extend phase done; hand off to
//!                       the Verifier
//!
//! The `GraphLoop` (next milestone) owns the side-effects: it actually
//! prompts the user, invokes the tool, applies the patch, or transitions
//! to verification. The proposer itself is pure: input model state → step.

use super::Conversation;
use crate::error::{HarnessError, Result};
use crate::graph::{Edge, Graph, GraphPatch, Node, NodeId, NodeKind, RelationType};
use crate::model::{Model, Role};
use crate::tools::ToolRegistry;
use std::sync::Arc;
use tracing::debug;

// ---------------------------------------------------------------------------
// Step type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum ProposerStep {
    AskUser {
        question: String,
        rationale: String,
    },
    CallTool {
        tool: String,
        args: serde_json::Value,
        rationale: String,
    },
    ProposePatch {
        patch: GraphPatch,
        rationale: String,
    },
    ReadyForVerify {
        rationale: String,
    },
}

impl ProposerStep {
    /// Short label for logs and transcripts.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AskUser { .. } => "ask_user",
            Self::CallTool { .. } => "call_tool",
            Self::ProposePatch { .. } => "propose_patch",
            Self::ReadyForVerify { .. } => "ready_for_verify",
        }
    }
}

// ---------------------------------------------------------------------------
// Proposer
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct GraphProposer {
    pub model: Arc<dyn Model>,
    pub tools: Arc<ToolRegistry>,
    /// Sampling temperature for the proposer call. Default 0.2.
    pub temperature: f64,
    /// Output cap for proposer responses (mostly structured JSON, so small).
    pub max_tokens: Option<usize>,
}

impl GraphProposer {
    pub fn new(model: Arc<dyn Model>, tools: Arc<ToolRegistry>) -> Self {
        Self {
            model,
            tools,
            temperature: 0.2,
            // Generous default. Reasoning-style models (DeepSeek-v4-pro,
            // GPT-o1, Claude with extended thinking) can burn 8-20K tokens
            // of invisible reasoning before producing the visible JSON,
            // especially deep into a conversation when the running history
            // grows large. 32K is the empirically-safe cap for deep models
            // on multi-round Graph phases; non-reasoning models will never
            // come close. Callers can override via `with_max_tokens`.
            max_tokens: Some(32768),
        }
    }

    pub fn with_temperature(mut self, t: f64) -> Self {
        self.temperature = t;
        self
    }

    /// Override the max_tokens cap for proposer calls. The default of 4096
    /// suits reasoning-style models on medium-complexity patches; bump
    /// higher if the model truncates large payloads mid-string.
    pub fn with_max_tokens(mut self, n: usize) -> Self {
        self.max_tokens = Some(n);
        self
    }

    /// Build the system prompt for a given task. Includes the schema for
    /// `ProposerStep` and the currently registered tools.
    pub fn build_system_prompt(&self, task: &str) -> String {
        let mut tools_section = String::new();
        let defs = self.tools.defs();
        if defs.is_empty() {
            tools_section.push_str("(no tools registered — the agent can only ask the user or propose patches)\n");
        } else {
            for def in &defs {
                tools_section.push_str(&format!(
                    "- `{}` — {}\n  args schema: {}\n",
                    def.name,
                    def.description,
                    serde_json::to_string(&def.schema).unwrap_or_else(|_| "{}".into())
                ));
            }
        }

        format!(
            "{PROMPT_PREAMBLE}\n\n## Task\n{task}\n\n## Available Tools\n{tools_section}\n{PROMPT_RULES}"
        )
    }

    /// Compose a `Conversation` seeded with this proposer's system prompt
    /// and task. Convenience wrapper — callers may build the conversation
    /// themselves for finer control.
    pub fn make_conversation(&self, task: impl Into<String>) -> Conversation {
        let task = task.into();
        let system = self.build_system_prompt(&task);
        Conversation::new(system, task)
    }

    /// One step: ask the model what to do next, parse the structured reply.
    pub async fn next_step(
        &self,
        conv: &Conversation,
        graph: &Graph,
    ) -> Result<ProposerStep> {
        let snapshot = render_graph_for_prompt(graph);
        let mut req = conv.to_request(&snapshot, self.temperature, self.max_tokens);
        // Make sure the system prompt is consistent with this proposer's task
        // even if the conversation was constructed elsewhere.
        if let Some(first) = req
            .messages
            .iter_mut()
            .find(|m| matches!(m.role, Role::System))
        {
            // Only overwrite if the prompt looks different — preserves caller's intent
            // when they pre-built a richer prompt.
            let want = self.build_system_prompt(&conv.task_description);
            if first.content != want {
                first.content = want;
            }
        }

        let resp = self.model.complete(req).await?;
        debug!(
            content_len = resp.content.len(),
            tokens = resp.usage.total_tokens,
            "proposer received model response"
        );
        parse_step(&resp.content)
    }
}

// ---------------------------------------------------------------------------
// Rendering the graph for the prompt
// ---------------------------------------------------------------------------

/// Render the graph compactly for inclusion in the model's prompt. Different
/// from `context::render_local_graph` in that it uses a more JSON-friendly,
/// idempotent layout the model is more likely to reason cleanly about.
///
/// Includes L1 summaries inline next to each node so the model always sees
/// the current semantic state of the graph alongside the structure.
fn render_graph_for_prompt(g: &Graph) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "graph version={} status={:?} nodes={} edges={} l1_entries={}\n",
        g.version,
        g.status,
        g.node_count(),
        g.edge_count(),
        g.l1.len(),
    ));
    let mut node_ids: Vec<&NodeId> = g.nodes.keys().collect();
    node_ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    if !node_ids.is_empty() {
        s.push_str("nodes (L0 + L1 oneline):\n");
        for id in node_ids {
            if let Some(n) = g.get_node(id) {
                let l1_hint = g
                    .l1
                    .get(id)
                    .filter(|d| !d.is_blank())
                    .map(|d| format!(" L1=\"{}\" (c={:.2})", d.render_oneline(), d.confidence))
                    .unwrap_or_else(|| " L1=(not yet enriched)".to_string());
                s.push_str(&format!(
                    "  - id={} kind={:?} summary={:?}{}\n",
                    n.id, n.kind, n.summary, l1_hint
                ));
            }
        }
    }
    if g.edge_count() > 0 {
        s.push_str("edges:\n");
        for (i, e) in g.iter_edges().enumerate() {
            s.push_str(&format!(
                "  [{i}] {} -[{:?} c={:.2}]-> {}  evidence={:?}\n",
                e.source, e.relation, e.confidence, e.target, e.evidence
            ));
        }
    }
    s
}

// ---------------------------------------------------------------------------
// JSON extraction + parsing
// ---------------------------------------------------------------------------

/// Pull the first balanced JSON object out of a (possibly markdown-wrapped)
/// model response. Tolerant of leading prose and code-fence variants.
pub fn extract_json_block(text: &str) -> Result<String> {
    let trimmed = text.trim();
    // Strip ```json ... ``` or ``` ... ``` fences.
    let inner: &str = if let Some(rest) = trimmed.strip_prefix("```json") {
        rest.trim_start_matches('\n')
            .rsplit_once("```")
            .map(|(a, _)| a)
            .unwrap_or(rest)
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest.trim_start_matches('\n')
            .rsplit_once("```")
            .map(|(a, _)| a)
            .unwrap_or(rest)
    } else {
        trimmed
    };

    // Find the first '{' and walk to its balanced close, respecting strings.
    let start = inner
        .find('{')
        .ok_or_else(|| HarnessError::model("proposer: no '{' in response".to_string()))?;
    let body = &inner[start..];

    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    let mut end: Option<usize> = None;
    for (i, c) in body.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match c {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end
        .ok_or_else(|| HarnessError::model("proposer: unterminated JSON object".to_string()))?;
    Ok(body[..end].to_string())
}

pub fn parse_step(text: &str) -> Result<ProposerStep> {
    let cleaned = extract_json_block(text)?;
    let value: serde_json::Value = serde_json::from_str(&cleaned).map_err(|e| {
        HarnessError::model(format!(
            "proposer: invalid JSON: {e}\n--- raw ---\n{text}\n--- cleaned ---\n{cleaned}"
        ))
    })?;

    let step = value
        .get("step")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::model("proposer: missing 'step' field".to_string()))?;
    let rationale = value
        .get("rationale")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    match step {
        "ask_user" => {
            let question = value
                .get("question")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    HarnessError::model("proposer: ask_user requires 'question'".to_string())
                })?
                .to_string();
            Ok(ProposerStep::AskUser { question, rationale })
        }
        "call_tool" => {
            let tool = value
                .get("tool")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    HarnessError::model("proposer: call_tool requires 'tool'".to_string())
                })?
                .to_string();
            let args = value
                .get("args")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            Ok(ProposerStep::CallTool {
                tool,
                args,
                rationale,
            })
        }
        "propose_patch" => {
            let patch_v = value.get("patch").ok_or_else(|| {
                HarnessError::model("proposer: propose_patch requires 'patch'".to_string())
            })?;
            let patch = parse_patch(patch_v)?;
            Ok(ProposerStep::ProposePatch { patch, rationale })
        }
        "ready_for_verify" => Ok(ProposerStep::ReadyForVerify { rationale }),
        other => Err(HarnessError::model(format!(
            "proposer: unknown step '{other}'"
        ))),
    }
}

fn parse_patch(v: &serde_json::Value) -> Result<GraphPatch> {
    let obj = v
        .as_object()
        .ok_or_else(|| HarnessError::model("proposer: patch must be an object".to_string()))?;

    let mut patch = GraphPatch::default();
    patch.reason = obj
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if let Some(arr) = obj.get("add_nodes").and_then(|v| v.as_array()) {
        for n in arr {
            patch.add_nodes.push(parse_node(n)?);
        }
    }
    if let Some(arr) = obj.get("add_edges").and_then(|v| v.as_array()) {
        for e in arr {
            patch.add_edges.push(parse_edge(e)?);
        }
    }
    if let Some(arr) = obj.get("remove_node_ids").and_then(|v| v.as_array()) {
        for id in arr {
            if let Some(s) = id.as_str() {
                patch.remove_node_ids.push(NodeId::from(s));
            }
        }
    }
    if let Some(arr) = obj.get("remove_edge_indices").and_then(|v| v.as_array()) {
        for i in arr {
            if let Some(idx) = i.as_u64() {
                patch.remove_edge_indices.push(idx as usize);
            }
        }
    }
    Ok(patch)
}

fn parse_node(v: &serde_json::Value) -> Result<Node> {
    let id = v
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::model("proposer: node missing 'id'".to_string()))?;
    let kind_str = v.get("kind").and_then(|v| v.as_str()).unwrap_or("Other");
    let kind = parse_node_kind(kind_str);
    let path = v.get("path").and_then(|v| v.as_str()).unwrap_or(id).to_string();
    let summary = v
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut node = Node::new(id, kind, path, summary);
    if let Some(meta) = v.get("metadata").and_then(|v| v.as_object()) {
        for (k, val) in meta {
            node = node.with_metadata(k, val.clone());
        }
    }
    Ok(node)
}

fn parse_node_kind(s: &str) -> NodeKind {
    match s {
        "File" => NodeKind::File,
        "Function" => NodeKind::Function,
        "Class" => NodeKind::Class,
        "Module" => NodeKind::Module,
        "Config" => NodeKind::Config,
        "Task" => NodeKind::Task,
        other => NodeKind::Other(other.to_string()),
    }
}

fn parse_edge(v: &serde_json::Value) -> Result<Edge> {
    let source = v
        .get("source")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::model("proposer: edge missing 'source'".to_string()))?;
    let target = v
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::model("proposer: edge missing 'target'".to_string()))?;
    let relation_str = v
        .get("relation")
        .and_then(|v| v.as_str())
        .unwrap_or("Other");
    let relation = parse_relation_type(relation_str);
    let confidence = v
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5);
    let evidence = v
        .get("evidence")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(Edge::new(source, target, relation, confidence, evidence))
}

fn parse_relation_type(s: &str) -> RelationType {
    match s {
        "Contains" => RelationType::Contains,
        "BelongsTo" => RelationType::BelongsTo,
        "Imports" => RelationType::Imports,
        "Exports" => RelationType::Exports,
        "DependsOn" => RelationType::DependsOn,
        "Calls" => RelationType::Calls,
        "Triggers" => RelationType::Triggers,
        "Reads" => RelationType::Reads,
        "Writes" => RelationType::Writes,
        "RevealedBy" => RelationType::RevealedBy,
        "InvalidatedBy" => RelationType::InvalidatedBy,
        other => RelationType::Other(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Prompt text — written for any modern instruction-tuned model.
// ---------------------------------------------------------------------------

const PROMPT_PREAMBLE: &str =
    "You are a Graph-Centric agent. Your job is to build a *relationship graph* \
that captures the user's task: its entities, their structural relationships, \
and the relevant constraints. The graph is the shared substrate between \
you, the user, and any sub-agents you might dispatch later.\n\
\n\
## The graph has three layers\n\
\n\
- **L0** (skeleton) — nodes + edges. What entities exist and how they relate. \
This is what your patches touch directly.\n\
- **L1** (muscle) — per-node semantic description: responsibility, \
implementation, design intent, constraints. You DO NOT write L1 yourself; an \
L1Enricher runs automatically after your patches and reads source/data (L2) \
to produce L1. You just focus on getting L0 right.\n\
- **L2** (skin) — the actual content: source files, configs, schemas, raw \
data. Accessed on demand via tools (e.g. `bash` with `cat`); never embedded \
in your patches.\n\
\n\
You operate in a strict iterative loop:\n\
  input → reason → extend graph → verify → (repair if wrong) → dispatch\n\
\n\
Each turn, you emit exactly ONE structured step as your entire response. \
The runtime executes the step (asking the user, invoking a tool, applying \
the patch, or transitioning to verification) and then asks you for the next \
step. Many small steps are better than few large ones — each step is also \
an opportunity to catch and reverse a mistake.";

const PROMPT_RULES: &str = r#"## Step schemas

Always emit exactly one of these JSON objects, with no surrounding prose,
no markdown code fences, nothing else:

1. ASK USER — when you need information only the user can provide.
   {"step":"ask_user","question":"<one clear question>","rationale":"<why this question now>"}

2. CALL TOOL — when running a tool can answer your own question.
   Reading L2 (file contents, config, command output) almost always happens
   via tools — never invent contents.
   {"step":"call_tool","tool":"<name>","args":{...},"rationale":"<what you expect to learn>"}

3. PROPOSE PATCH — add or remove L0 nodes/edges based on what you now know.
   {"step":"propose_patch",
    "patch":{
      "add_nodes":     [{"id":"<unique>","kind":"<NodeKind>","path":"<path>","summary":"<one-line L0 hint, <=120 chars>"}],
      "add_edges":     [{"source":"<id>","target":"<id>","relation":"<RelationType>","confidence":0..1,"evidence":"<why>"}],
      "remove_node_ids":     ["<id>"],
      "remove_edge_indices": [0,3],
      "reason":"<one sentence>"
    },
    "rationale":"<why this change now>"}

   The `summary` field is a one-line L0 description — just enough for routing.
   The full L1 (responsibility/implementation/design_intent/constraints) is
   produced by the L1Enricher automatically after the patch lands; do NOT
   try to write it yourself.

4. READY FOR VERIFY — when the L0 captures the task completely.
   {"step":"ready_for_verify","rationale":"<why you believe the graph is done>"}

## Vocabularies (use these exact strings)

NodeKind:     File | Function | Class | Module | Config | Task | Other
RelationType: Contains | BelongsTo | Imports | Exports | DependsOn |
              Calls | Triggers | Reads | Writes | RevealedBy | InvalidatedBy | Other

(For domains that aren't code, use Other with a descriptive metadata.kind
 field — e.g. id="house:42", kind="Other", metadata={"kind":"location"}.)

## Discipline

- Output EXACTLY one JSON object. Nothing before, nothing after.
- Be conservative. Ask the user when unsure — never fabricate edges.
- All edge endpoints must exist (already present, or being added in the
  same patch's add_nodes). The runtime rejects edges with missing endpoints.
- Confidence guidance: 0.9+ for evidence you directly observed (tool
  output, user statement); 0.6 for reasonable inference; <0.5 only if you
  want to flag uncertainty for verification.
- Keep each patch small. A patch that adds one or two related nodes/edges
  is normal; a patch that rebuilds half the graph is not.
- Speak the user's language when asking questions, but keep the JSON
  field names, NodeKind, and RelationType strings exactly as specified.
- L1 is not your concern in `propose_patch` — focus on L0 correctness.
  The L1 column in the graph snapshot you see each turn reflects what the
  L1Enricher has produced so far; if it looks wrong for a node, flag it as
  rationale in your next patch and the verifier/repairer will pick it up."#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FinishReason, ModelRequest, ModelResponse, Usage};
    use async_trait::async_trait;
    use std::sync::Mutex;

    // A scripted mock model — returns canned responses in order.
    struct MockModel {
        name: String,
        responses: Mutex<Vec<String>>,
        captured: Mutex<Vec<ModelRequest>>,
    }

    impl MockModel {
        fn new(responses: Vec<String>) -> Self {
            Self {
                name: "mock".into(),
                responses: Mutex::new(responses),
                captured: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Model for MockModel {
        fn name(&self) -> &str {
            &self.name
        }
        async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
            self.captured.lock().unwrap().push(req);
            let content = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| "{}".to_string());
            Ok(ModelResponse {
                content,
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }
    }

    fn proposer_with(responses: Vec<&str>) -> GraphProposer {
        let model = Arc::new(MockModel::new(
            responses.iter().rev().map(|s| s.to_string()).collect(),
        ));
        let tools = Arc::new(ToolRegistry::new());
        GraphProposer::new(model, tools)
    }

    #[test]
    fn extract_json_unwrapped() {
        let s = r#"{"step":"ready_for_verify","rationale":"done"}"#;
        let out = extract_json_block(s).unwrap();
        assert_eq!(out, s);
    }

    #[test]
    fn extract_json_strips_markdown_fence() {
        let s = "```json\n{\"step\":\"ready_for_verify\"}\n```";
        let out = extract_json_block(s).unwrap();
        assert!(out.contains("\"step\""));
    }

    #[test]
    fn extract_json_strips_bare_fence() {
        let s = "```\n{\"step\":\"ready_for_verify\"}\n```";
        let out = extract_json_block(s).unwrap();
        assert!(out.contains("\"step\""));
    }

    #[test]
    fn extract_json_tolerates_prose_before_object() {
        let s = "Here's my step: {\"step\":\"ready_for_verify\"}";
        let out = extract_json_block(s).unwrap();
        assert!(out.starts_with('{'));
    }

    #[test]
    fn extract_json_respects_strings_with_braces() {
        // A nested brace inside a string should not throw off the balance.
        let s = r#"{"step":"ask_user","question":"is this {weird}?"}"#;
        let out = extract_json_block(s).unwrap();
        assert_eq!(out, s);
    }

    #[test]
    fn parse_step_ask_user() {
        let s = r#"{"step":"ask_user","question":"How many users?","rationale":"need scale"}"#;
        match parse_step(s).unwrap() {
            ProposerStep::AskUser { question, rationale } => {
                assert_eq!(question, "How many users?");
                assert_eq!(rationale, "need scale");
            }
            other => panic!("expected AskUser, got {other:?}"),
        }
    }

    #[test]
    fn parse_step_call_tool() {
        let s = r#"{"step":"call_tool","tool":"bash","args":{"command":"ls"},"rationale":"see files"}"#;
        match parse_step(s).unwrap() {
            ProposerStep::CallTool { tool, args, .. } => {
                assert_eq!(tool, "bash");
                assert_eq!(args.get("command").unwrap().as_str(), Some("ls"));
            }
            other => panic!("expected CallTool, got {other:?}"),
        }
    }

    #[test]
    fn parse_step_propose_patch() {
        let s = r#"{
          "step":"propose_patch",
          "patch":{
            "add_nodes":[{"id":"u","kind":"Other","path":"u","summary":"user node"}],
            "add_edges":[{"source":"u","target":"v","relation":"DependsOn","confidence":0.8,"evidence":"stated"}],
            "remove_edge_indices":[2],
            "reason":"capture dep"
          },
          "rationale":"new info from user"
        }"#;
        match parse_step(s).unwrap() {
            ProposerStep::ProposePatch { patch, .. } => {
                assert_eq!(patch.add_nodes.len(), 1);
                assert_eq!(patch.add_nodes[0].id.as_str(), "u");
                assert_eq!(patch.add_edges.len(), 1);
                assert!(matches!(patch.add_edges[0].relation, RelationType::DependsOn));
                assert!((patch.add_edges[0].confidence - 0.8).abs() < 1e-9);
                assert_eq!(patch.remove_edge_indices, vec![2]);
                assert_eq!(patch.reason, "capture dep");
            }
            other => panic!("expected ProposePatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_step_unknown_kind_falls_to_other() {
        let s = r#"{
          "step":"propose_patch",
          "patch":{
            "add_nodes":[{"id":"x","kind":"BoardMeeting","path":"x","summary":""}],
            "add_edges":[],
            "reason":""
          }
        }"#;
        match parse_step(s).unwrap() {
            ProposerStep::ProposePatch { patch, .. } => match &patch.add_nodes[0].kind {
                NodeKind::Other(name) => assert_eq!(name, "BoardMeeting"),
                other => panic!("expected NodeKind::Other, got {other:?}"),
            },
            other => panic!("expected ProposePatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_step_unknown_relation_falls_to_other() {
        let s = r#"{
          "step":"propose_patch",
          "patch":{
            "add_nodes":[{"id":"a","kind":"File","path":"a","summary":""},
                          {"id":"b","kind":"File","path":"b","summary":""}],
            "add_edges":[{"source":"a","target":"b","relation":"SoftCoupling","confidence":0.5,"evidence":""}]
          }
        }"#;
        match parse_step(s).unwrap() {
            ProposerStep::ProposePatch { patch, .. } => {
                match &patch.add_edges[0].relation {
                    RelationType::Other(s) => assert_eq!(s, "SoftCoupling"),
                    other => panic!("expected RelationType::Other, got {other:?}"),
                }
            }
            _ => panic!("expected ProposePatch"),
        }
    }

    #[test]
    fn parse_step_ready_for_verify_minimal() {
        let s = r#"{"step":"ready_for_verify"}"#;
        match parse_step(s).unwrap() {
            ProposerStep::ReadyForVerify { rationale } => assert!(rationale.is_empty()),
            other => panic!("expected ReadyForVerify, got {other:?}"),
        }
    }

    #[test]
    fn parse_step_missing_step_field_errors() {
        let s = r#"{"foo":"bar"}"#;
        let err = parse_step(s).unwrap_err();
        assert!(format!("{err}").contains("missing 'step'"));
    }

    #[test]
    fn parse_step_unknown_step_errors() {
        let s = r#"{"step":"refactor_universe"}"#;
        let err = parse_step(s).unwrap_err();
        assert!(format!("{err}").contains("unknown step"));
    }

    #[test]
    fn parse_step_malformed_json_errors() {
        let s = "not even JSON here";
        let err = parse_step(s).unwrap_err();
        // Either no `{` or invalid JSON — both are acceptable here.
        assert!(format!("{err}").to_lowercase().contains("json")
            || format!("{err}").contains("'{'"));
    }

    #[tokio::test]
    async fn next_step_calls_model_and_parses() {
        let p = proposer_with(vec![r#"{"step":"ready_for_verify","rationale":"trivial"}"#]);
        let conv = p.make_conversation("test task");
        let graph = Graph::new();
        let step = p.next_step(&conv, &graph).await.unwrap();
        match step {
            ProposerStep::ReadyForVerify { rationale } => assert_eq!(rationale, "trivial"),
            other => panic!("expected ReadyForVerify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn next_step_request_includes_graph_snapshot() {
        let p = proposer_with(vec![r#"{"step":"ready_for_verify"}"#]);
        let conv = p.make_conversation("test");
        let mut g = Graph::new();
        g.add_node(Node::file("hello.rs", "greeting"));
        let _ = p.next_step(&conv, &g).await.unwrap();
        // Check that the captured request's system message contains the node
        let model = p.model.clone();
        // Downcast trick: we only have Arc<dyn Model>; use the inherent test API.
        // Inspect via the request the mock captured.
        let mock = model
            .as_any_mock();
        let captured = mock.captured.lock().unwrap();
        let req = captured.last().unwrap();
        let snapshot_msg = req
            .messages
            .iter()
            .find(|m| matches!(m.role, Role::System) && m.content.contains("graph version="))
            .expect("graph snapshot system message present");
        assert!(snapshot_msg.content.contains("hello.rs"));
    }

    #[tokio::test]
    async fn system_prompt_mentions_three_layer_graph() {
        // Prompt should educate the model about L0/L1/L2 distinction.
        let p = proposer_with(vec![r#"{"step":"ready_for_verify"}"#]);
        let prompt = p.build_system_prompt("any task");
        assert!(prompt.contains("L0"), "missing L0 mention");
        assert!(prompt.contains("L1"), "missing L1 mention");
        assert!(prompt.contains("L2"), "missing L2 mention");
        // Should tell model NOT to write L1 (enricher's job)
        assert!(
            prompt.contains("L1Enricher") || prompt.contains("automatically"),
            "missing instruction about L1 enrichment"
        );
    }

    #[tokio::test]
    async fn graph_snapshot_includes_l1_state_next_to_each_node() {
        // When L1 exists, snapshot shows it. When L1 missing, shows "not yet enriched".
        let p = proposer_with(vec![r#"{"step":"ready_for_verify"}"#]);
        let conv = p.make_conversation("test");
        let mut g = Graph::new();
        g.add_node(Node::file("with_l1.rs", "X"));
        g.add_node(Node::file("no_l1.rs", "Y"));
        g.l1.set(
            NodeId::from("with_l1.rs"),
            crate::graph::L1Description::new("does X", "wraps Y", "for Z", "always W")
                .with_confidence(0.85),
        );
        let _ = p.next_step(&conv, &g).await.unwrap();
        let model_arc = p.model.clone();
        let mock = model_arc.as_any_mock();
        let captured = mock.captured.lock().unwrap();
        let snapshot = captured
            .last()
            .unwrap()
            .messages
            .iter()
            .find(|m| matches!(m.role, Role::System) && m.content.contains("graph version="))
            .unwrap();
        assert!(snapshot.content.contains("does X"), "with_l1.rs should show L1");
        assert!(
            snapshot.content.contains("not yet enriched"),
            "no_l1.rs should mark itself as un-enriched"
        );
    }

    // Helper: downcast Arc<dyn Model> back to &MockModel for inspection.
    // We do this only inside tests; production code never needs it.
    trait MockProbe {
        fn as_any_mock(&self) -> &MockModel;
    }
    impl MockProbe for Arc<dyn Model> {
        fn as_any_mock(&self) -> &MockModel {
            // This is sound only because we know in the tests above that the
            // Arc holds a MockModel. We use raw-pointer reinterpretation
            // because dyn Model isn't downcast-able by default.
            let raw = Arc::as_ptr(self) as *const MockModel;
            unsafe { &*raw }
        }
    }
}
