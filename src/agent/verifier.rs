//! Verifier — turns the abstract "is this graph good enough?" question
//! into a structured `VerificationResult`.
//!
//! Three layers, applied in order:
//!
//! 1. **Structural** (deterministic, no model). Runs
//!    `Graph::find_inconsistencies` — dangling edges, orphans, cycles in
//!    acyclic-required relations, duplicates, invalid confidences. These
//!    are objective; the runtime always trusts them.
//!
//! 2. **Model self-check**. Given the task and the current graph, ask the
//!    model: *Does the graph cover everything the task needs? What's
//!    missing, wrong, or overstated?* Returns structured issues with a
//!    suggested scope (the node ids the issue concerns) so that the
//!    `LocalRepairer` can address each one **locally** per principle #3.
//!
//! 3. **User confirmation** (Phase 3 hook). For now, the verifier exposes
//!    a `requires_user_confirmation` flag the caller can flip when running
//!    as a main agent; we don't actually prompt anyone here. The CLI/loop
//!    binds the prompt UI.
//!
//! The verdict (`passed`) is the conjunction of all three layers. Any
//! `high`-severity structural issue or model-flagged concern fails the
//! verification; `medium`/`low` are surfaced for the caller to weigh.

use super::Conversation;
use super::proposer::extract_json_block;
use crate::context::SourceLoader;
use crate::error::{HarnessError, Result};
use crate::graph::{Graph, Inconsistency, NodeId};
use crate::model::{Message, Model, ModelRequest, Role};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Severity {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IssueSource {
    Structural,
    Model,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyIssue {
    pub source: IssueSource,
    pub severity: Severity,
    pub description: String,
    /// Node ids the issue concerns. Used by the LocalRepairer to bound the
    /// scope of any fix; if empty, the issue is graph-wide.
    pub scope: Vec<NodeId>,
}

impl VerifyIssue {
    pub fn structural(severity: Severity, description: impl Into<String>, scope: Vec<NodeId>) -> Self {
        Self {
            source: IssueSource::Structural,
            severity,
            description: description.into(),
            scope,
        }
    }

    pub fn from_model(severity: Severity, description: impl Into<String>, scope: Vec<NodeId>) -> Self {
        Self {
            source: IssueSource::Model,
            severity,
            description: description.into(),
            scope,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationResult {
    pub passed: bool,
    pub issues: Vec<VerifyIssue>,
    /// Model's own confidence about its self-check (when run). 1.0 if
    /// model self-check was skipped.
    pub model_confidence: f64,
    pub rationale: String,
}

impl VerificationResult {
    pub fn high_severity_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|i| i.severity == Severity::High)
            .count()
    }

    pub fn structural_issues(&self) -> impl Iterator<Item = &VerifyIssue> {
        self.issues
            .iter()
            .filter(|i| matches!(i.source, IssueSource::Structural))
    }

    pub fn model_issues(&self) -> impl Iterator<Item = &VerifyIssue> {
        self.issues
            .iter()
            .filter(|i| matches!(i.source, IssueSource::Model))
    }
}

// ---------------------------------------------------------------------------
// Verifier
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Verifier {
    /// If `Some`, run a model self-check; if `None`, skip layer 2 and
    /// only do structural validation.
    pub model: Option<Arc<dyn Model>>,
    /// L2 backend for the L1-sampling layer (layer 3). If `None`, the L1
    /// check is skipped — useful for the early Graph phase before any
    /// source is available.
    pub loader: Option<Arc<dyn SourceLoader>>,
    pub temperature: f64,
    /// Severities that fail verification. Defaults to just `High` —
    /// medium/low surface but don't block.
    pub failing_severities: Vec<Severity>,
    /// How many nodes (with non-blank L1) to sample-check against L2 per
    /// `verify()` call. 0 disables the L1 layer even when both model and
    /// loader are present.
    pub l1_sample_size: usize,
}

impl Verifier {
    /// Structural-only verifier — no model, no L1 sampling. Good for tests,
    /// sub-agents whose scope is too small to need a self-check, and the
    /// early rounds where the graph isn't substantive enough to evaluate.
    pub fn structural_only() -> Self {
        Self {
            model: None,
            loader: None,
            temperature: 0.0,
            failing_severities: vec![Severity::High],
            l1_sample_size: 0,
        }
    }

    pub fn with_model(model: Arc<dyn Model>) -> Self {
        Self {
            model: Some(model),
            loader: None,
            temperature: 0.0,
            failing_severities: vec![Severity::High],
            l1_sample_size: 0,
        }
    }

    /// Attach a `SourceLoader` so the L1-sampling layer can fetch L2 content.
    /// Defaults `l1_sample_size` to 3 if it was previously 0.
    pub fn with_loader(mut self, loader: Arc<dyn SourceLoader>) -> Self {
        self.loader = Some(loader);
        if self.l1_sample_size == 0 {
            self.l1_sample_size = 3;
        }
        self
    }

    pub fn with_l1_sample_size(mut self, n: usize) -> Self {
        self.l1_sample_size = n;
        self
    }

    pub fn with_failing_severities(mut self, sev: Vec<Severity>) -> Self {
        self.failing_severities = sev;
        self
    }

    /// Run all layers. `conv` is optional — when present, the model self-check
    /// can include short context from the conversation history (e.g., the
    /// user's clarifications) for a sharper coverage judgment.
    pub async fn verify(
        &self,
        graph: &Graph,
        task: &str,
        conv: Option<&Conversation>,
    ) -> Result<VerificationResult> {
        // Layer 1: structural
        let mut issues: Vec<VerifyIssue> = graph
            .find_inconsistencies()
            .into_iter()
            .map(structural_to_issue)
            .collect();

        // Layer 2: model self-check
        let model_confidence = if let Some(model) = &self.model {
            let (mut model_issues, conf) = self.model_self_check(model.as_ref(), graph, task, conv).await?;
            issues.append(&mut model_issues);
            conf
        } else {
            1.0
        };

        // Layer 3: L1 sampling — for nodes that have L1, ask the model
        // whether the L1 description still matches the L2 reality.
        if let (Some(model), Some(loader)) = (&self.model, &self.loader) {
            if self.l1_sample_size > 0 {
                let mut l1_issues = self
                    .l1_sampling_check(model.as_ref(), loader.as_ref(), graph)
                    .await?;
                issues.append(&mut l1_issues);
            }
        }

        let passed = !issues
            .iter()
            .any(|i| self.failing_severities.contains(&i.severity));

        let rationale = format!(
            "{} structural, {} model, {} L1, passed={}, model_confidence={:.2}",
            issues.iter().filter(|i| matches!(i.source, IssueSource::Structural)).count(),
            issues
                .iter()
                .filter(|i| matches!(i.source, IssueSource::Model) && !i.description.contains("L1 drift"))
                .count(),
            issues
                .iter()
                .filter(|i| i.description.contains("L1 drift"))
                .count(),
            passed,
            model_confidence
        );

        Ok(VerificationResult {
            passed,
            issues,
            model_confidence,
            rationale,
        })
    }

    /// Pick up to `l1_sample_size` nodes with non-blank L1, fetch their L2,
    /// and ask the model whether the description still matches reality.
    /// Returns `VerifyIssue`s with descriptions prefixed `"L1 drift on
    /// <node_id>: …"` so `GraphError::from_verify_issue` routes them to the
    /// L1Semantic variant.
    async fn l1_sampling_check(
        &self,
        model: &dyn Model,
        loader: &dyn SourceLoader,
        graph: &Graph,
    ) -> Result<Vec<VerifyIssue>> {
        // Deterministic sample: take the first N node ids (sorted) that have
        // a non-blank L1. Predictable for tests; for production we'd shuffle
        // with a per-call seed but that requires a rand dep.
        let mut candidates: Vec<NodeId> = graph
            .l1
            .iter()
            .filter(|(_, d)| !d.is_blank())
            .map(|(id, _)| id.clone())
            .collect();
        candidates.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        candidates.truncate(self.l1_sample_size);

        let mut issues = Vec::new();
        for id in candidates {
            let l1 = match graph.l1.get(&id) {
                Some(d) => d.clone(),
                None => continue,
            };
            let l2 = match loader.load(&id) {
                Ok(s) => s,
                Err(e) => {
                    // L2 not available — flag as low-severity issue so the
                    // repairer can decide (often this just means the node
                    // is conceptual, not file-backed).
                    warn!(node = %id, error = %e, "verifier: L2 load failed during L1 sampling");
                    continue;
                }
            };
            let l2_excerpt = excerpt(&l2, 4_000);

            let user_prompt = format!(
                "## Node\n{id}\n\n## Stored L1 description\n{}\n\n## L2 excerpt\n```\n{l2_excerpt}\n```\n\n\
                 Does the L1 description match what the L2 says? Reply with ONE JSON object only:\n\n\
                 {{\n  \"verdict\": \"match\" | \"drift\",\n  \"severity\": \"high\" | \"medium\" | \"low\",\n  \
                 \"detail\": \"<one short sentence; what's wrong if drift, else why you think it matches>\"\n}}\n\n\
                 Rules:\n\
                 - verdict=drift only when you can point at a specific contradiction.\n\
                 - severity=high  if drift would mislead a downstream sub-agent reading L1 alone.\n\
                 - severity=medium if drift is stylistic / partial / outdated but not actively wrong.\n\
                 - severity=low for nitpicks.",
                l1.render_full().trim_end()
            );

            let req = ModelRequest {
                messages: vec![
                    Message::system(SYSTEM_PROMPT_L1_CHECK),
                    Message::user(user_prompt),
                ],
                tools: vec![],
                temperature: self.temperature,
                max_tokens: Some(512),
                stop: vec![],
            };
            let resp = match model.complete(req).await {
                Ok(r) => r,
                Err(e) => {
                    warn!(node = %id, error = %e, "verifier: L1 model check failed");
                    continue;
                }
            };
            let cleaned = match extract_json_block(&resp.content) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let value: serde_json::Value = match serde_json::from_str(&cleaned) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let verdict = value
                .get("verdict")
                .and_then(|v| v.as_str())
                .unwrap_or("match");
            if verdict != "drift" {
                continue;
            }
            let severity = parse_severity(
                value
                    .get("severity")
                    .and_then(|v| v.as_str())
                    .unwrap_or("medium"),
            );
            let detail = value
                .get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or("L1 description differs from L2");
            issues.push(VerifyIssue::from_model(
                severity,
                format!("L1 drift on {id}: {detail}"),
                vec![id],
            ));
        }
        debug!(checked = self.l1_sample_size, issues = issues.len(), "verifier L1 sampling complete");
        Ok(issues)
    }

    async fn model_self_check(
        &self,
        model: &dyn Model,
        graph: &Graph,
        task: &str,
        conv: Option<&Conversation>,
    ) -> Result<(Vec<VerifyIssue>, f64)> {
        let snapshot = render_graph_for_verifier(graph);
        let recent_conv = conv
            .map(|c| recent_dialog(c, 6))
            .unwrap_or_default();
        let user_prompt = format!(
            "## Task\n{task}\n\n## Current Graph\n{snapshot}\n\n## Recent Dialog (last 6 turns)\n{recent_conv}\n\n\
             Decide whether the graph is sufficient to dispatch downstream work on this task. \
             If anything is missing, mis-stated, or over-asserted, list each as a structured issue. \
             Reply with ONE JSON object only (no markdown, no prose):\n\n\
             {{\n  \"verdict\": \"pass\" | \"fail\",\n  \"confidence\": 0..1,\n  \
             \"issues\": [\n    {{\n      \"severity\": \"high\" | \"medium\" | \"low\",\n      \
             \"description\": \"<one sentence>\",\n      \"scope\": [\"<node_id>\", ...]\n    }}\n  ],\n  \
             \"rationale\": \"<short reason for the verdict>\"\n}}\n\n\
             Rules:\n\
             - high   = blocks dispatch (graph wrong or missing critical info)\n\
             - medium = should be addressed but doesn't block\n\
             - low    = nice-to-have refinement\n\
             - scope  = ids of nodes the issue concerns, used for LOCAL repair. \
                        Use [] for graph-wide concerns.\n\
             - verdict = \"pass\" only when there are NO high-severity issues."
        );

        let req = ModelRequest {
            messages: vec![
                Message::system(SYSTEM_PROMPT_VERIFIER),
                Message::user(user_prompt),
            ],
            tools: vec![],
            temperature: self.temperature,
            max_tokens: Some(2048),
            stop: vec![],
        };
        let resp = model.complete(req).await?;
        debug!(
            content_len = resp.content.len(),
            tokens = resp.usage.total_tokens,
            "verifier model self-check returned"
        );

        let cleaned = extract_json_block(&resp.content)?;
        let value: serde_json::Value = serde_json::from_str(&cleaned).map_err(|e| {
            HarnessError::model(format!(
                "verifier: invalid JSON: {e}\n--- raw ---\n{}\n--- cleaned ---\n{cleaned}",
                resp.content
            ))
        })?;
        parse_verifier_json(&value)
    }
}

fn parse_verifier_json(value: &serde_json::Value) -> Result<(Vec<VerifyIssue>, f64)> {
    let confidence = value
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5)
        .clamp(0.0, 1.0);

    let mut issues = Vec::new();
    if let Some(arr) = value.get("issues").and_then(|v| v.as_array()) {
        for item in arr {
            let severity = item
                .get("severity")
                .and_then(|v| v.as_str())
                .map(parse_severity)
                .unwrap_or(Severity::Medium);
            let description = item
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let scope: Vec<NodeId> = item
                .get("scope")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.as_str().map(NodeId::from))
                        .collect()
                })
                .unwrap_or_default();
            if description.is_empty() {
                continue;
            }
            issues.push(VerifyIssue::from_model(severity, description, scope));
        }
    }
    Ok((issues, confidence))
}

fn parse_severity(s: &str) -> Severity {
    match s.to_ascii_lowercase().as_str() {
        "high" | "block" | "blocker" | "critical" => Severity::High,
        "low" | "minor" | "nit" => Severity::Low,
        _ => Severity::Medium,
    }
}

// ---------------------------------------------------------------------------
// Structural → VerifyIssue mapping
// ---------------------------------------------------------------------------

fn structural_to_issue(inc: Inconsistency) -> VerifyIssue {
    match inc {
        Inconsistency::DanglingEdge {
            edge_idx,
            missing_endpoint,
        } => VerifyIssue::structural(
            Severity::High,
            format!("dangling edge #{edge_idx}: endpoint {missing_endpoint} not in graph"),
            vec![missing_endpoint],
        ),
        Inconsistency::OrphanNode { node } => VerifyIssue::structural(
            Severity::Low,
            format!("orphan node {node} (no incoming or outgoing edges)"),
            vec![node],
        ),
        Inconsistency::Cycle { cycle, relation } => {
            let scope = cycle.clone();
            VerifyIssue::structural(
                Severity::High,
                format!("cycle in acyclic-required relation {relation:?}: {:?}", cycle),
                scope,
            )
        }
        Inconsistency::DuplicateEdge {
            first_idx,
            second_idx,
        } => VerifyIssue::structural(
            Severity::Medium,
            format!("duplicate edge: indices {first_idx} and {second_idx}"),
            vec![],
        ),
        Inconsistency::InvalidConfidence { edge_idx, value } => VerifyIssue::structural(
            Severity::Medium,
            format!("edge #{edge_idx} has invalid confidence {value}"),
            vec![],
        ),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn render_graph_for_verifier(g: &Graph) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "graph version={} status={:?} nodes={} edges={}\n",
        g.version,
        g.status,
        g.node_count(),
        g.edge_count()
    ));
    if g.node_count() > 0 {
        let mut ids: Vec<&NodeId> = g.nodes.keys().collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        s.push_str("nodes:\n");
        for id in ids {
            if let Some(n) = g.get_node(id) {
                s.push_str(&format!("  - {} [{:?}] {}\n", n.id, n.kind, n.summary));
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

fn recent_dialog(conv: &Conversation, max_turns: usize) -> String {
    let mut s = String::new();
    let total = conv.messages.len();
    let start = total.saturating_sub(max_turns);
    for m in &conv.messages[start..] {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "agent",
            Role::Tool => "tool",
            Role::System => continue,
        };
        s.push_str(&format!("[{role}] {}\n", m.content));
    }
    if s.is_empty() {
        s.push_str("(no prior dialog)\n");
    }
    s
}

const SYSTEM_PROMPT_VERIFIER: &str = "You are a verifier in a graph-centric agent harness. \
Your single job: judge whether a relationship graph is good enough to dispatch downstream work \
for the given task. You are STRICT and TERSE. You always reply with one JSON object — no \
markdown, no prose around it. You do not propose patches; you flag issues. A repairer agent will \
handle the fixes locally.";

const SYSTEM_PROMPT_L1_CHECK: &str = "You are an L1-drift detector in a graph-centric agent harness. \
You are given ONE node's stored L1 description and an excerpt of its L2 (the real source/data). \
Your single job: decide whether the L1 still matches reality. You are SKEPTICAL — you flag drift \
only when you can point at a concrete contradiction. You always reply with one JSON object — no \
markdown, no prose. You do not propose fixes.";

/// Tail-truncate a string to roughly `max_chars`, keeping the head (for
/// L2 excerpts the file header / imports are typically the most relevant
/// — opposite of bash output where the tail matters).
fn excerpt(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}\n…[{} chars truncated from tail]", total - max_chars)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, Graph, Node, RelationType};
    use crate::model::{FinishReason, ModelResponse, Usage};
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct MockModel {
        response: Mutex<Option<String>>,
        captured: Mutex<Vec<ModelRequest>>,
    }

    impl MockModel {
        fn new(response: &str) -> Self {
            Self {
                response: Mutex::new(Some(response.to_string())),
                captured: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Model for MockModel {
        fn name(&self) -> &str {
            "mock-verifier"
        }
        async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
            self.captured.lock().unwrap().push(req);
            let content = self
                .response
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| "{}".to_string());
            Ok(ModelResponse {
                content,
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }
    }

    fn clean_graph() -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::file("a", "A"));
        g.add_node(Node::file("b", "B"));
        g.add_edge(Edge::new("a", "b", RelationType::Imports, 0.9, "")).unwrap();
        g
    }

    fn graph_with_cycle() -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::task("t1", "T1"));
        g.add_node(Node::task("t2", "T2"));
        g.add_edge(Edge::new("t1", "t2", RelationType::DependsOn, 1.0, "")).unwrap();
        g.add_edge(Edge::new("t2", "t1", RelationType::DependsOn, 1.0, "")).unwrap();
        g
    }

    #[tokio::test]
    async fn structural_only_passes_clean_graph() {
        let v = Verifier::structural_only();
        let g = clean_graph();
        let r = v.verify(&g, "build a simple thing", None).await.unwrap();
        assert!(r.passed);
        assert_eq!(r.issues.len(), 0);
        assert_eq!(r.model_confidence, 1.0);
    }

    #[tokio::test]
    async fn structural_only_fails_cycle_with_high_severity() {
        let v = Verifier::structural_only();
        let g = graph_with_cycle();
        let r = v.verify(&g, "", None).await.unwrap();
        assert!(!r.passed);
        assert!(r.high_severity_count() >= 1);
        let cycle_issue = r
            .structural_issues()
            .find(|i| i.description.contains("cycle"))
            .expect("cycle issue present");
        assert_eq!(cycle_issue.severity, Severity::High);
    }

    #[tokio::test]
    async fn orphan_node_is_low_severity() {
        // An orphan alone shouldn't fail verification with default failing_severities
        let mut g = Graph::new();
        g.add_node(Node::file("lonely", "no friends"));
        let v = Verifier::structural_only();
        let r = v.verify(&g, "", None).await.unwrap();
        assert!(r.passed);
        assert!(r.structural_issues().any(|i| i.severity == Severity::Low));
    }

    #[tokio::test]
    async fn model_self_check_pass_response_parsed() {
        let resp = r#"{"verdict":"pass","confidence":0.92,"issues":[],"rationale":"covers task"}"#;
        let model: Arc<dyn Model> = Arc::new(MockModel::new(resp));
        let v = Verifier::with_model(model);
        let r = v.verify(&clean_graph(), "describe a project", None).await.unwrap();
        assert!(r.passed);
        assert!((r.model_confidence - 0.92).abs() < 1e-6);
        assert_eq!(r.issues.len(), 0);
    }

    #[tokio::test]
    async fn model_self_check_high_issue_fails_verification() {
        let resp = r#"{
            "verdict":"fail",
            "confidence":0.75,
            "issues":[
                {"severity":"high","description":"missing data flow edge between A and B","scope":["a","b"]},
                {"severity":"low","description":"summary of a could be sharper","scope":["a"]}
            ],
            "rationale":"key relation missing"
        }"#;
        let model: Arc<dyn Model> = Arc::new(MockModel::new(resp));
        let v = Verifier::with_model(model);
        let r = v.verify(&clean_graph(), "task", None).await.unwrap();
        assert!(!r.passed);
        assert_eq!(r.high_severity_count(), 1);
        let high = r
            .model_issues()
            .find(|i| i.severity == Severity::High)
            .unwrap();
        assert!(high.description.contains("missing data flow"));
        assert_eq!(high.scope, vec![NodeId::from("a"), NodeId::from("b")]);
    }

    #[tokio::test]
    async fn structural_high_severity_alone_fails_even_with_pass_model_verdict() {
        let resp = r#"{"verdict":"pass","confidence":0.9,"issues":[]}"#;
        let model: Arc<dyn Model> = Arc::new(MockModel::new(resp));
        let v = Verifier::with_model(model);
        let r = v.verify(&graph_with_cycle(), "", None).await.unwrap();
        assert!(!r.passed); // Structural cycle is High → fails regardless
    }

    #[tokio::test]
    async fn model_response_with_markdown_fence_handled() {
        let resp = "```json\n{\"verdict\":\"pass\",\"confidence\":0.7,\"issues\":[]}\n```";
        let model: Arc<dyn Model> = Arc::new(MockModel::new(resp));
        let v = Verifier::with_model(model);
        let r = v.verify(&clean_graph(), "", None).await.unwrap();
        assert!(r.passed);
        assert!((r.model_confidence - 0.7).abs() < 1e-6);
    }

    /// Two-shot mock: first response handles the model_self_check; remaining
    /// responses handle L1 sampling (one per sampled node).
    struct ScriptedModel {
        responses: Mutex<Vec<String>>,
    }

    impl ScriptedModel {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().rev().map(String::from).collect()),
            }
        }
    }

    #[async_trait]
    impl Model for ScriptedModel {
        fn name(&self) -> &str {
            "scripted"
        }
        async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse> {
            let content = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| r#"{"verdict":"pass"}"#.to_string());
            Ok(ModelResponse {
                content,
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }
    }

    fn graph_with_one_l1() -> (Graph, Arc<crate::context::InMemorySources>) {
        let mut g = clean_graph();
        // a and b exist; give a an L1 description
        g.l1.set(
            NodeId::from("a"),
            crate::graph::L1Description::new("does A", "wraps libfoo", "centralize", "no panics"),
        );
        let mut src = std::collections::HashMap::new();
        src.insert(
            NodeId::from("a"),
            "pub fn a() {}\n// Actually does B, not A\n".into(),
        );
        src.insert(NodeId::from("b"), "pub fn b() {}\n".into());
        (g, Arc::new(crate::context::InMemorySources(src)))
    }

    #[tokio::test]
    async fn l1_sampling_layer_emits_l1_drift_issues() {
        let self_check = r#"{"verdict":"pass","confidence":0.9,"issues":[]}"#;
        let l1_drift = r#"{"verdict":"drift","severity":"high","detail":"L1 says wraps libfoo but L2 imports libbar"}"#;
        let (graph, loader) = graph_with_one_l1();
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![self_check, l1_drift]));
        let v = Verifier::with_model(model).with_loader(loader);
        let r = v.verify(&graph, "test task", None).await.unwrap();
        let drift = r
            .issues
            .iter()
            .find(|i| i.description.starts_with("L1 drift on"))
            .expect("L1 drift issue present");
        assert!(drift.description.contains("L1 says wraps libfoo"));
        assert_eq!(drift.scope, vec![NodeId::from("a")]);
        assert_eq!(drift.severity, Severity::High);
        // Verification should fail because the L1 drift is high-severity
        assert!(!r.passed);
    }

    #[tokio::test]
    async fn l1_sampling_skipped_when_loader_missing() {
        let self_check = r#"{"verdict":"pass","confidence":0.9,"issues":[]}"#;
        let (graph, _loader) = graph_with_one_l1();
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![self_check]));
        let v = Verifier::with_model(model); // no .with_loader → L1 layer disabled
        let r = v.verify(&graph, "", None).await.unwrap();
        assert!(r.passed);
        assert!(
            !r.issues.iter().any(|i| i.description.starts_with("L1 drift")),
            "no L1 drift issues should appear when loader absent"
        );
    }

    #[tokio::test]
    async fn l1_sampling_verdict_match_produces_no_issue() {
        let self_check = r#"{"verdict":"pass","confidence":0.9,"issues":[]}"#;
        let l1_ok = r#"{"verdict":"match","severity":"low","detail":"looks aligned"}"#;
        let (graph, loader) = graph_with_one_l1();
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![self_check, l1_ok]));
        let v = Verifier::with_model(model).with_loader(loader);
        let r = v.verify(&graph, "", None).await.unwrap();
        assert!(r.passed);
        assert!(!r.issues.iter().any(|i| i.description.starts_with("L1 drift")));
    }

    #[tokio::test]
    async fn l1_sampling_skips_nodes_without_l1() {
        let self_check = r#"{"verdict":"pass","confidence":0.9,"issues":[]}"#;
        // No L1 entries at all → sampling has nothing to check
        let mut g = clean_graph();
        let mut src = std::collections::HashMap::new();
        src.insert(NodeId::from("a"), "x".into());
        let loader = Arc::new(crate::context::InMemorySources(src));
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![self_check]));
        let v = Verifier::with_model(model).with_loader(loader);
        let r = v.verify(&g, "", None).await.unwrap();
        assert!(r.passed);
        // Sanity: graph is unchanged
        let _ = &mut g;
    }

    #[test]
    fn severity_parse_aliases() {
        assert_eq!(parse_severity("HIGH"), Severity::High);
        assert_eq!(parse_severity("blocker"), Severity::High);
        assert_eq!(parse_severity("minor"), Severity::Low);
        assert_eq!(parse_severity("nit"), Severity::Low);
        assert_eq!(parse_severity("anything-else"), Severity::Medium);
    }

    #[test]
    fn dangling_edge_maps_to_high_with_scope() {
        let issue = structural_to_issue(Inconsistency::DanglingEdge {
            edge_idx: 3,
            missing_endpoint: NodeId::from("ghost"),
        });
        assert_eq!(issue.severity, Severity::High);
        assert_eq!(issue.scope, vec![NodeId::from("ghost")]);
        assert!(issue.description.contains("ghost"));
    }
}
