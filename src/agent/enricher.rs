//! L1Enricher — model reads L2, writes L1.
//!
//! For each node lacking a meaningful L1 description (or with stale L1),
//! `L1Enricher` loads the node's L2 (raw content) via a [`SourceLoader`],
//! prompts the model with that content plus the node's L0 context, and
//! parses a structured [`L1Description`] out of the response.
//!
//! ### Where this sits in the loop
//!
//! Per design doc v2.0, L1 enrichment is triggered:
//! - Once in the initial Graph phase, after the model has agreed on the
//!   L0 skeleton (the proposer's `ReadyForVerify` transition).
//! - Incrementally whenever L0 patches add new nodes — those new nodes get
//!   enriched before verification.
//! - On demand when the Verifier's L1-sampling layer (Phase 2.5.6) flags
//!   that a node's L1 has drifted from its L2.
//!
//! All three call sites use [`L1Enricher::enrich_node`] for one node or
//! [`L1Enricher::enrich_missing`] / [`L1Enricher::enrich_low_confidence`]
//! for batches.
//!
//! ### What this is NOT
//!
//! - Not a graph builder — it doesn't add/remove nodes or edges; it only
//!   writes to `Graph::l1`.
//! - Not a structural verifier — that's the [`super::Verifier`].
//! - Not domain-aware — the prompt is generic; the model decides what
//!   "responsibility / implementation / design_intent / constraints" mean
//!   for the domain at hand.

use super::proposer::extract_json_block;
use crate::context::SourceLoader;
use crate::error::{HarnessError, Result};
use crate::graph::{Graph, L1Description, NodeId};
use crate::model::{Message, Model, ModelRequest};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Maximum L2 characters injected into the enrichment prompt per node.
/// Larger files get tail-truncated (the same convention as
/// `tools::truncate_tail`). 12K chars ≈ 3K tokens, leaving room for
/// system prompt + neighborhood + response budget within a 128K window.
const DEFAULT_L2_CHAR_CAP: usize = 12_000;

/// How many immediate neighbors to include in the L0 context block.
const DEFAULT_NEIGHBOR_LIMIT: usize = 12;

/// Hard cap on L1 confidence when L2 was unavailable (inference-only path).
const DEFAULT_L0_ONLY_CONFIDENCE_CAP: f64 = 0.6;

#[derive(Clone)]
pub struct L1Enricher {
    pub model: Arc<dyn Model>,
    pub loader: Arc<dyn SourceLoader>,
    pub temperature: f64,
    pub max_tokens: Option<usize>,
    pub l2_char_cap: usize,
    pub neighbor_limit: usize,
    /// Hard cap on L1 confidence when L2 was unavailable (inference-only
    /// path). Default 0.6, matching the pre-config behavior.
    pub l0_only_confidence_cap: f64,
}

impl L1Enricher {
    pub fn new(model: Arc<dyn Model>, loader: Arc<dyn SourceLoader>) -> Self {
        Self {
            model,
            loader,
            temperature: 0.2,
            max_tokens: Some(1024),
            l2_char_cap: DEFAULT_L2_CHAR_CAP,
            neighbor_limit: DEFAULT_NEIGHBOR_LIMIT,
            l0_only_confidence_cap: DEFAULT_L0_ONLY_CONFIDENCE_CAP,
        }
    }

    pub fn with_l2_char_cap(mut self, cap: usize) -> Self {
        self.l2_char_cap = cap;
        self
    }

    pub fn with_neighbor_limit(mut self, n: usize) -> Self {
        self.neighbor_limit = n;
        self
    }

    pub fn with_l0_only_confidence_cap(mut self, cap: f64) -> Self {
        self.l0_only_confidence_cap = cap;
        self
    }

    /// Enrich a single node. Reads L2, prompts model, returns the parsed
    /// L1Description.
    ///
    /// **L2-available path** — when `loader.load()` returns content, the
    /// model gets the full prompt (L0 + neighbors + L2 excerpt) and may
    /// return a high-confidence description directly grounded in source.
    ///
    /// **Fallback path** — when `loader.load()` errors (e.g.
    /// [`NullSourceLoader`](crate::context::NullSourceLoader) for abstract
    /// tasks, or just a missing file), the enricher *does not* fail.
    /// Instead it asks the model to **infer L1 from L0 alone**: the
    /// node's metadata + immediate L0 neighbors + the task hint. The
    /// returned confidence is capped at 0.6 to reflect that this L1 is
    /// inferred, not observed.
    ///
    /// Either way the result is the parsed L1Description; the caller
    /// decides whether to persist.
    pub async fn enrich_node(
        &self,
        graph: &Graph,
        node_id: &NodeId,
        task_hint: &str,
    ) -> Result<L1Description> {
        let node = graph
            .get_node(node_id)
            .ok_or_else(|| HarnessError::enricher(format!("enricher: node {node_id} not in graph")))?;

        // Try L2; if it fails, switch to the inferential prompt rather than
        // bailing out. This keeps abstract / planning tasks from getting
        // stuck at "0 L1 entries forever".
        let l2_result = self.loader.load(node_id);
        let (l2_text, has_l2) = match l2_result {
            Ok(text) if !text.trim().is_empty() => (tail_truncate(&text, self.l2_char_cap), true),
            Ok(_) => (String::new(), false),
            Err(e) => {
                debug!(
                    node = %node_id,
                    error = %e,
                    "enricher: L2 unavailable; falling back to L0-only inference"
                );
                (String::new(), false)
            }
        };

        let mut neighbors: Vec<String> = Vec::new();
        for e in graph.outgoing(node_id).take(self.neighbor_limit) {
            neighbors.push(format!("→ [{:?}] {}", e.relation, e.target));
        }
        for e in graph.incoming(node_id).take(self.neighbor_limit) {
            neighbors.push(format!("← [{:?}] {}", e.relation, e.source));
        }
        let neighbor_block = if neighbors.is_empty() {
            "(no immediate neighbors in L0)".to_string()
        } else {
            neighbors.join("\n  ")
        };

        let user_prompt = if has_l2 {
            format!(
                "## Node\nid: {id}\nkind: {kind:?}\npath: {path}\nL0 summary: {summary}\n\n\
                 ## L0 immediate neighbors\n  {neighbor_block}\n\n\
                 ## L2 content (raw)\n```\n{l2_truncated}\n```\n\n\
                 ## Task hint (why we're enriching this node)\n{task_hint}\n\n\
                 Produce one L1Description JSON object describing this node. Schema:\n\n\
                 {{\n  \"responsibility\": \"<one sentence: what is this FOR>\",\n  \
                 \"implementation\": \"<HOW it does what it does — strategy, deps, shape>\",\n  \
                 \"design_intent\": \"<WHY designed this way — motivation behind non-obvious choices>\",\n  \
                 \"constraints\": \"<important invariants / things that MUST hold>\",\n  \
                 \"confidence\": <float 0..1>\n}}\n\n\
                 Confidence guidance:\n\
                 - 0.9+ : L2 was clear and you wrote each field from direct observation\n\
                 - 0.6  : L2 was partial; some inference was needed\n\
                 - 0.3  : L2 was too short/ambiguous to be sure — flag for re-enrichment\n\n\
                 Output the JSON object ONLY. No markdown fences, no surrounding prose.",
                id = node.id,
                kind = node.kind,
                path = node.path,
                summary = node.summary,
                neighbor_block = neighbor_block,
                l2_truncated = l2_text,
                task_hint = if task_hint.trim().is_empty() {
                    "(no specific task focus)"
                } else {
                    task_hint
                }
            )
        } else {
            // L0-only inferential prompt.
            format!(
                "## Node\nid: {id}\nkind: {kind:?}\npath: {path}\nL0 summary: {summary}\n\n\
                 ## L0 immediate neighbors\n  {neighbor_block}\n\n\
                 ## L2 status\nUnavailable — this node has no direct source/data to read.\n\
                 Infer L1 from: the L0 summary + metadata, the neighbors above, and the task.\n\n\
                 ## Task hint (why we're enriching this node)\n{task_hint}\n\n\
                 Produce one L1Description JSON object. Schema:\n\n\
                 {{\n  \"responsibility\": \"<one sentence: what is this FOR>\",\n  \
                 \"implementation\": \"<typical / expected HOW given the node's kind and role>\",\n  \
                 \"design_intent\": \"<WHY this node likely exists in the task's larger picture>\",\n  \
                 \"constraints\": \"<invariants implied by the node's role>\",\n  \
                 \"confidence\": <float 0..0.6>\n}}\n\n\
                 IMPORTANT — L0-only inference rules:\n\
                 - You have NO L2 evidence. Do not invent specific implementation details \
                   that would require source. Speak at the level of role, not mechanism.\n\
                 - Confidence MUST stay at 0.6 or below (the runtime caps it anyway, \
                   but be honest in the value you write).\n\
                 - If the L0 context is too sparse to infer anything meaningful, return \
                   a description with confidence 0.3 and brief, generic fields rather \
                   than fabricating specifics.\n\n\
                 Output the JSON object ONLY. No markdown fences, no surrounding prose.",
                id = node.id,
                kind = node.kind,
                path = node.path,
                summary = node.summary,
                neighbor_block = neighbor_block,
                task_hint = if task_hint.trim().is_empty() {
                    "(no specific task focus)"
                } else {
                    task_hint
                }
            )
        };

        let system_prompt = if has_l2 {
            load_prompt_file("skills/prompts/enricher.md", SYSTEM_PROMPT_ENRICHER)
        } else {
            load_prompt_file("skills/prompts/enricher-no-l2.md", SYSTEM_PROMPT_ENRICHER_NO_L2)
        };

        let req = ModelRequest {
            messages: vec![
                Message::system(system_prompt),
                Message::user(user_prompt),
            ],
            tools: vec![enricher_tool_schema()],
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            stop: vec![],
        };
        let resp = self.model.complete(req).await?;
        debug!(
            node = %node_id,
            has_l2,
            content_len = resp.content.len(),
            reasoning_len = resp.reasoning_content.as_deref().map(str::len).unwrap_or(0),
            tool_calls = resp.tool_calls.len(),
            tokens = resp.usage.total_tokens,
            "enricher model response"
        );

        // Strategy A: prefer native tool_calls; fall back to text.
        let parse_text = if let Some(args) = parse_enricher_from_tool_calls(&resp.tool_calls) {
            args
        } else {
            // Reasoning-model fallback (DeepSeek / M3). See
            // ModelResponse::text_or_reasoning; db2d993d regression.
            resp.text_or_reasoning().to_string()
        };
        let mut desc = parse_l1_description(&parse_text)?;
        if !has_l2 {
            // Hard-cap confidence on the L0-only path regardless of what
            // the model claimed. The model promised to stay ≤ 0.6 but we
            // enforce it server-side as the source of truth.
            desc.confidence = desc.confidence.min(self.l0_only_confidence_cap);
        }
        Ok(desc)
    }

    /// For each id in `ids`, enrich and write to `graph.l1`. Skips ids
    /// that already have a non-blank L1 with confidence ≥ `min_confidence`.
    /// Returns the number of nodes actually enriched.
    pub async fn enrich_missing(
        &self,
        graph: &mut Graph,
        ids: &[NodeId],
        task_hint: &str,
        min_confidence: f64,
    ) -> Result<usize> {
        let mut enriched = 0usize;
        for id in ids {
            let needs = match graph.l1.get(id) {
                None => true,
                Some(d) => d.is_blank() || d.confidence < min_confidence,
            };
            if !needs {
                continue;
            }
            match self.enrich_node(graph, id, task_hint).await {
                Ok(desc) => {
                    graph.l1.set(id.clone(), desc);
                    enriched += 1;
                }
                Err(e) => {
                    warn!(node = %id, error = %e, "enricher: skipping node after error");
                }
            }
        }
        info!(enriched, total = ids.len(), "enricher: batch complete");
        Ok(enriched)
    }

    /// Re-enrich every node whose stored L1 confidence falls below
    /// `threshold`. Useful after L0 changes that may have invalidated
    /// existing L1 descriptions.
    pub async fn enrich_low_confidence(
        &self,
        graph: &mut Graph,
        threshold: f64,
        task_hint: &str,
    ) -> Result<usize> {
        let ids = graph.l1.low_confidence(threshold);
        self.enrich_missing(graph, &ids, task_hint, threshold).await
    }
}

// ---------------------------------------------------------------------------
// JSON parsing
// ---------------------------------------------------------------------------

fn parse_l1_description(text: &str) -> Result<L1Description> {
    let cleaned = extract_json_block(text)?;
    let value: serde_json::Value = serde_json::from_str(&cleaned).map_err(|e| {
        HarnessError::enricher(format!(
            "enricher: invalid JSON: {e}\n--- raw ---\n{text}\n--- cleaned ---\n{cleaned}"
        ))
    })?;

    let responsibility = value
        .get("responsibility")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let implementation = value
        .get("implementation")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let design_intent = value
        .get("design_intent")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let constraints = value
        .get("constraints")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let confidence = value
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5);

    // Reject totally-empty descriptions — the model produced nothing useful.
    if responsibility.trim().is_empty()
        && implementation.trim().is_empty()
        && design_intent.trim().is_empty()
        && constraints.trim().is_empty()
    {
        return Err(HarnessError::enricher(
            "enricher: model returned an empty L1Description; rejecting",
        ));
    }

    Ok(L1Description::new(
        responsibility,
        implementation,
        design_intent,
        constraints,
    )
    .with_confidence(confidence))
}

/// Tool schema for L1 enrichment. The model emits a structured
/// `write_l1_description` call with the four field values; falls back
/// to text when absent (db2d993d-class reasoning-only responses).
fn enricher_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "write_l1_description",
            "description": "Write a structured L1 description (responsibility / implementation / design_intent / constraints + confidence) for one graph node.",
            "parameters": {
                "type": "object",
                "properties": {
                    "responsibility": {"type": "string", "description": "What this node is responsible for."},
                    "implementation": {"type": "string", "description": "How it carries out that responsibility."},
                    "design_intent": {"type": "string", "description": "Why it's shaped this way."},
                    "constraints": {"type": "string", "description": "Hard constraints / non-negotiables."},
                    "confidence": {"type": "number", "description": "0..1. 0.6+ only when L2 was available; cap to 0.6 on the L0-only path."}
                },
                "required": ["responsibility"]
            }
        }
    })
}

/// Convert a tool_call's structured args back to the JSON text the
/// legacy parser consumes, so the surrounding code path stays
/// unchanged. Returns None when no matching tool_call is present.
fn parse_enricher_from_tool_calls(
    tool_calls: &[crate::model::ToolCall],
) -> Option<String> {
    let tc = tool_calls.iter().find(|tc| tc.name == "write_l1_description")?;
    Some(tc.arguments.to_string())
}

// ---------------------------------------------------------------------------
// Tail truncation — local copy to avoid pulling tools::* into agent::
// (the helper in tools/mod.rs prepends a marker; here we want a tail-only
// view sized to a char cap with a brief truncation note.)
// ---------------------------------------------------------------------------

fn tail_truncate(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let skip = total - max_chars;
    let tail: String = text.chars().skip(skip).collect();
    format!("[…{skip} chars truncated from head…]\n{tail}")
}

// ---------------------------------------------------------------------------
// Prompt text
// ---------------------------------------------------------------------------

/// Try to load a prompt from a file, falling back to the hardcoded default.
/// This lets users edit `skills/prompts/enricher-*.md` without recompiling.
fn load_prompt_file(path: &str, default: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| default.to_string())
}

const SYSTEM_PROMPT_ENRICHER: &str = "You are an L1 enrichment agent in a graph-centric agent harness. \
Your job is to read a node's raw L2 content and produce a precise, structured semantic description \
(L1) of that node. You write four fields: responsibility, implementation, design_intent, constraints, \
plus a confidence. You output exactly one JSON object — no markdown, no prose. You are TERSE: each \
field is one short sentence (one short paragraph at most). You do not invent things L2 does not \
support; when L2 is ambiguous, lower your confidence accordingly.";

const SYSTEM_PROMPT_ENRICHER_NO_L2: &str = "You are an L1 enrichment agent in a graph-centric agent harness, \
working in INFERENCE mode: the node you're describing has no L2 source data, so you must reason from \
the node's L0 metadata (id, kind, path, summary), its immediate L0 neighbors, and the task description. \
You write four fields plus confidence, exactly as in the normal mode, BUT: (1) speak at the level of \
role and purpose rather than concrete mechanism — you have no evidence for specific implementation \
details, (2) keep confidence ≤ 0.6 always; the runtime caps it anyway, (3) if L0 context is too sparse \
to say anything specific, return brief generic fields with confidence 0.3 rather than fabricate. You \
output exactly one JSON object — no markdown, no prose.";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::InMemorySources;
    use crate::graph::{Edge, Node, RelationType};
    use crate::model::{FinishReason, ModelResponse, Usage};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct ScriptedModel {
        responses: Mutex<Vec<String>>,
        captured: Mutex<Vec<ModelRequest>>,
    }

    impl ScriptedModel {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().rev().map(String::from).collect()),
                captured: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Model for ScriptedModel {
        fn name(&self) -> &str {
            "scripted-enricher"
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
                reasoning_content: None,
                usage: Usage::default(),
            })
        }
    }

    fn make_graph_with_neighbors() -> (Graph, Arc<InMemorySources>) {
        let mut g = Graph::new();
        g.add_node(Node::file("a.rs", "module A"));
        g.add_node(Node::file("b.rs", "module B"));
        g.add_node(Node::file("c.rs", "module C"));
        g.add_edge(Edge::new("a.rs", "b.rs", RelationType::Imports, 0.8, "use b"))
            .unwrap();
        g.add_edge(Edge::new("c.rs", "a.rs", RelationType::Calls, 0.7, "call a"))
            .unwrap();

        let mut src = HashMap::new();
        src.insert(
            NodeId::from("a.rs"),
            "pub fn handle() {}\n// A handles requests\n".into(),
        );
        src.insert(NodeId::from("b.rs"), "pub fn helper() {}\n".into());
        src.insert(NodeId::from("c.rs"), "pub fn caller() {}\n".into());
        (g, Arc::new(InMemorySources(src)))
    }

    #[tokio::test]
    async fn enrich_node_parses_and_returns_description() {
        let resp = r#"{
            "responsibility": "Handle inbound requests",
            "implementation": "Calls into module B for helpers",
            "design_intent": "Single entry point keeps routing simple",
            "constraints": "Must not call C directly",
            "confidence": 0.85
        }"#;
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![resp]));
        let (graph, loader) = make_graph_with_neighbors();
        let enricher = L1Enricher::new(model, loader);
        let desc = enricher
            .enrich_node(&graph, &NodeId::from("a.rs"), "analyze request handling")
            .await
            .unwrap();
        assert_eq!(desc.responsibility, "Handle inbound requests");
        assert_eq!(desc.implementation, "Calls into module B for helpers");
        assert!((desc.confidence - 0.85).abs() < 1e-9);
        assert!(!desc.is_blank());
    }

    #[tokio::test]
    async fn enrich_node_handles_markdown_fence() {
        let resp = "```json\n{\"responsibility\":\"r\",\"implementation\":\"\",\"design_intent\":\"\",\"constraints\":\"\",\"confidence\":0.7}\n```";
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![resp]));
        let (graph, loader) = make_graph_with_neighbors();
        let enricher = L1Enricher::new(model, loader);
        let desc = enricher
            .enrich_node(&graph, &NodeId::from("a.rs"), "")
            .await
            .unwrap();
        assert_eq!(desc.responsibility, "r");
    }

    #[tokio::test]
    async fn enrich_node_rejects_totally_empty_response() {
        let resp = r#"{"responsibility":"","implementation":"","design_intent":"","constraints":"","confidence":0.1}"#;
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![resp]));
        let (graph, loader) = make_graph_with_neighbors();
        let enricher = L1Enricher::new(model, loader);
        let err = enricher
            .enrich_node(&graph, &NodeId::from("a.rs"), "")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("empty L1Description"));
    }

    #[tokio::test]
    async fn enrich_node_falls_back_to_l0_inference_when_l2_unavailable() {
        // L2 missing → enricher uses fallback prompt; model returns an L1
        // describing the node from its L0 role alone.
        let resp = r#"{
            "responsibility":"likely the entry-point service",
            "implementation":"role-level inference; no concrete mechanism known",
            "design_intent":"central routing per the task description",
            "constraints":"role-bound only",
            "confidence":0.55
        }"#;
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![resp]));
        // Empty source map → load always fails
        let empty_src = Arc::new(InMemorySources(HashMap::new()));
        let mut g = Graph::new();
        g.add_node(Node::file("ghost.rs", "ghost node"));
        let enricher = L1Enricher::new(model, empty_src);
        let desc = enricher
            .enrich_node(&g, &NodeId::from("ghost.rs"), "build a thing")
            .await
            .unwrap();
        assert_eq!(desc.responsibility, "likely the entry-point service");
        assert!((desc.confidence - 0.55).abs() < 1e-9);
        assert!(!desc.is_blank());
    }

    #[tokio::test]
    async fn fallback_caps_confidence_at_0_6_even_if_model_claims_higher() {
        // Model misbehaves and returns 0.95 confidence on the L0-only path;
        // runtime must cap it to 0.6.
        let resp = r#"{
            "responsibility":"r",
            "implementation":"i",
            "design_intent":"d",
            "constraints":"c",
            "confidence":0.95
        }"#;
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![resp]));
        let empty_src = Arc::new(InMemorySources(HashMap::new()));
        let mut g = Graph::new();
        g.add_node(Node::file("x", "X"));
        let enricher = L1Enricher::new(model, empty_src);
        let desc = enricher
            .enrich_node(&g, &NodeId::from("x"), "")
            .await
            .unwrap();
        assert!((desc.confidence - 0.6).abs() < 1e-9, "confidence not capped: {}", desc.confidence);
    }

    #[tokio::test]
    async fn fallback_path_uses_inferential_system_prompt() {
        // Snoop the captured request to confirm the L0-only system prompt is
        // sent, not the regular L2 one.
        let resp = r#"{"responsibility":"r","implementation":"","design_intent":"","constraints":"","confidence":0.5}"#;
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![resp]));
        let empty_src = Arc::new(InMemorySources(HashMap::new()));
        let mut g = Graph::new();
        g.add_node(Node::file("x", "X"));
        let enricher = L1Enricher::new(model.clone(), empty_src);
        let _ = enricher
            .enrich_node(&g, &NodeId::from("x"), "task")
            .await
            .unwrap();

        let captured = {
            let model_arc = model.clone();
            let raw = Arc::as_ptr(&model_arc) as *const ScriptedModel;
            // Sound here: we know the Arc holds a ScriptedModel.
            let mock = unsafe { &*raw };
            mock.captured.lock().unwrap().clone()
        };
        let req = captured.last().expect("model was called");
        let sys = req
            .messages
            .iter()
            .find(|m| matches!(m.role, crate::model::Role::System))
            .expect("system message present");
        assert!(
            sys.content.contains("INFERENCE mode"),
            "expected L0-only system prompt, got: {}",
            sys.content
        );
        // User prompt should mention L2 being unavailable
        let user = req
            .messages
            .iter()
            .find(|m| matches!(m.role, crate::model::Role::User))
            .expect("user message present");
        assert!(
            user.content.contains("L2 status\nUnavailable"),
            "expected L2-unavailable hint in user prompt"
        );
    }

    #[tokio::test]
    async fn null_source_loader_triggers_fallback() {
        let resp = r#"{
            "responsibility":"abstract-task node",
            "implementation":"role-level",
            "design_intent":"part of the task plan",
            "constraints":"",
            "confidence":0.5
        }"#;
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![resp]));
        let loader = Arc::new(crate::context::NullSourceLoader);
        let mut g = Graph::new();
        g.add_node(Node::file("relocation", "plan node"));
        let enricher = L1Enricher::new(model, loader);
        let desc = enricher
            .enrich_node(&g, &NodeId::from("relocation"), "plan relocation")
            .await
            .unwrap();
        assert_eq!(desc.responsibility, "abstract-task node");
        assert!(desc.confidence <= 0.6);
    }

    #[tokio::test]
    async fn enrich_missing_with_null_loader_populates_l1_for_all_nodes() {
        // Two-node graph; NullSourceLoader → both nodes get L1 via fallback.
        let resp_a = r#"{"responsibility":"node A role","implementation":"","design_intent":"","constraints":"","confidence":0.55}"#;
        let resp_b = r#"{"responsibility":"node B role","implementation":"","design_intent":"","constraints":"","confidence":0.5}"#;
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![resp_a, resp_b]));
        let loader = Arc::new(crate::context::NullSourceLoader);
        let mut g = Graph::new();
        g.add_node(Node::file("a", "A"));
        g.add_node(Node::file("b", "B"));
        let enricher = L1Enricher::new(model, loader);
        let n = enricher
            .enrich_missing(&mut g, &[NodeId::from("a"), NodeId::from("b")], "task", 0.4)
            .await
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(g.l1.get(&NodeId::from("a")).unwrap().responsibility, "node A role");
        assert_eq!(g.l1.get(&NodeId::from("b")).unwrap().responsibility, "node B role");
    }

    #[tokio::test]
    async fn enrich_node_errors_when_node_missing() {
        let resp = r#"{"responsibility":"r"}"#;
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![resp]));
        let (graph, loader) = make_graph_with_neighbors();
        let enricher = L1Enricher::new(model, loader);
        let err = enricher
            .enrich_node(&graph, &NodeId::from("not_in_graph"), "")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("not in graph"));
    }

    #[tokio::test]
    async fn enrich_missing_writes_to_l1_store_and_counts() {
        let resp_a = r#"{"responsibility":"A handles","implementation":"","design_intent":"","constraints":"","confidence":0.8}"#;
        let resp_b = r#"{"responsibility":"B helps","implementation":"","design_intent":"","constraints":"","confidence":0.8}"#;
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![resp_a, resp_b]));
        let (mut graph, loader) = make_graph_with_neighbors();
        let enricher = L1Enricher::new(model, loader);
        let ids = vec![NodeId::from("a.rs"), NodeId::from("b.rs")];
        let n = enricher
            .enrich_missing(&mut graph, &ids, "analyze", 0.5)
            .await
            .unwrap();
        assert_eq!(n, 2);
        assert!(graph.l1.contains(&NodeId::from("a.rs")));
        assert!(graph.l1.contains(&NodeId::from("b.rs")));
        assert_eq!(graph.l1.get(&NodeId::from("a.rs")).unwrap().responsibility, "A handles");
    }

    #[tokio::test]
    async fn enrich_missing_skips_already_enriched_above_threshold() {
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![
            r#"{"responsibility":"new","implementation":"","design_intent":"","constraints":"","confidence":0.9}"#,
        ]));
        let (mut graph, loader) = make_graph_with_neighbors();
        // Pre-populate a high-confidence L1 for a.rs
        graph.l1.set(
            NodeId::from("a.rs"),
            L1Description::new("existing", "", "", "").with_confidence(0.95),
        );
        let enricher = L1Enricher::new(model, loader);
        let ids = vec![NodeId::from("a.rs"), NodeId::from("b.rs")];
        let n = enricher
            .enrich_missing(&mut graph, &ids, "", 0.5)
            .await
            .unwrap();
        // Only b.rs should have been touched; a.rs's existing L1 stays.
        assert_eq!(n, 1);
        assert_eq!(
            graph.l1.get(&NodeId::from("a.rs")).unwrap().responsibility,
            "existing"
        );
        assert!(graph.l1.contains(&NodeId::from("b.rs")));
    }

    #[tokio::test]
    async fn enrich_missing_re_enriches_low_confidence_entries() {
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![
            r#"{"responsibility":"refreshed","implementation":"","design_intent":"","constraints":"","confidence":0.9}"#,
        ]));
        let (mut graph, loader) = make_graph_with_neighbors();
        // Low-confidence entry for a.rs — should be re-enriched
        graph.l1.set(
            NodeId::from("a.rs"),
            L1Description::new("stale", "", "", "").with_confidence(0.3),
        );
        let enricher = L1Enricher::new(model, loader);
        let n = enricher
            .enrich_missing(&mut graph, &[NodeId::from("a.rs")], "", 0.6)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let updated = graph.l1.get(&NodeId::from("a.rs")).unwrap();
        assert_eq!(updated.responsibility, "refreshed");
        // Revision should bump since set() replaced existing
        assert_eq!(updated.revision, 2);
    }

    #[tokio::test]
    async fn enrich_low_confidence_targets_stored_threshold() {
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![
            r#"{"responsibility":"hi-c","implementation":"","design_intent":"","constraints":"","confidence":0.9}"#,
            r#"{"responsibility":"hi-c","implementation":"","design_intent":"","constraints":"","confidence":0.9}"#,
        ]));
        let (mut graph, loader) = make_graph_with_neighbors();
        graph.l1.set(
            NodeId::from("a.rs"),
            L1Description::new("lo", "", "", "").with_confidence(0.4),
        );
        graph.l1.set(
            NodeId::from("b.rs"),
            L1Description::new("ok", "", "", "").with_confidence(0.8),
        );
        let enricher = L1Enricher::new(model, loader);
        let n = enricher
            .enrich_low_confidence(&mut graph, 0.7, "")
            .await
            .unwrap();
        // Only a.rs (0.4 < 0.7) should be re-enriched
        assert_eq!(n, 1);
        assert_eq!(graph.l1.get(&NodeId::from("a.rs")).unwrap().responsibility, "hi-c");
        // b.rs stays
        assert_eq!(graph.l1.get(&NodeId::from("b.rs")).unwrap().responsibility, "ok");
    }

    #[test]
    fn tail_truncate_keeps_tail() {
        let s = "x".repeat(1000);
        let out = tail_truncate(&s, 100);
        assert!(out.starts_with("[…"));
        assert!(out.ends_with("x"));
    }

    #[test]
    fn tail_truncate_short_unchanged() {
        let out = tail_truncate("hello", 100);
        assert_eq!(out, "hello");
    }
}
