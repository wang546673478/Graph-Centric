//! LocalRepairer — single-issue, scope-bounded graph patches.
//!
//! Enforces design principle #3 (local graph repair, never bulk) **and**
//! the three-layer model (v2.0). The repairer dispatches by [`GraphError`]
//! variant:
//!
//! | Variant       | Path                                                             |
//! |---------------|------------------------------------------------------------------|
//! | `L0Structural`| Read L2 + ask model for a small L0 patch (add/remove edges/nodes)|
//! | `L1Semantic`  | Call `L1Enricher` to re-derive L1 for the drifted node           |
//! | `ScopeGap`    | Ask model to propose new nodes+edges that fill the missing region|
//!
//! In all three paths the output is a [`GraphPatch`] scoped narrowly to the
//! issue. The Graph-phase loop applies the patch and re-verifies.
//!
//! ## Why not return a `Graph` instead of a patch?
//!
//! Patches are reversible (the original graph + patch fully describes the
//! new state). Returning a whole graph would lose that audit trail and make
//! testing harder. The loop applies patches and bumps `Graph::version` so
//! each repair is a discrete event in the lineage.

use super::enricher::L1Enricher;
use super::graph_loop::GraphError;
use super::proposer::extract_json_block;
use super::verifier::VerifyIssue;
use crate::error::{HarnessError, Result};
use crate::graph::{Edge, Graph, GraphPatch, Node, NodeId, NodeKind, RelationType};
use crate::model::{Message, Model, ModelRequest};
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, warn};

#[derive(Clone)]
pub struct LocalRepairer {
    pub model: Arc<dyn Model>,
    /// How many hops around the issue's scope to include in the subgraph
    /// sent to the model. Default 1.
    pub neighborhood_depth: usize,
    pub temperature: f64,
    pub max_tokens: Option<usize>,
    /// Optional `L1Enricher` used by the L1Semantic repair path. When
    /// absent, L1Semantic repairs fail with an explicit error so the
    /// caller knows to either provide an enricher or skip these errors.
    pub l1_enricher: Option<L1Enricher>,
}

impl LocalRepairer {
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self {
            model,
            neighborhood_depth: 1,
            temperature: 0.1,
            max_tokens: Some(2048),
            l1_enricher: None,
        }
    }

    pub fn with_neighborhood_depth(mut self, depth: usize) -> Self {
        self.neighborhood_depth = depth;
        self
    }

    pub fn with_l1_enricher(mut self, enricher: L1Enricher) -> Self {
        self.l1_enricher = Some(enricher);
        self
    }

    // -----------------------------------------------------------------------
    // Public entry points
    // -----------------------------------------------------------------------

    /// Repair a single typed error. The preferred entry point.
    pub async fn repair_from_error(
        &self,
        graph: &Graph,
        err: &GraphError,
        task: &str,
    ) -> Result<GraphPatch> {
        match err {
            GraphError::L0Structural { .. } => self.repair_l0_structural(graph, err, task).await,
            GraphError::L1Semantic { node, .. } => self.repair_l1_semantic(graph, node, task).await,
            GraphError::ScopeGap { .. } => self.repair_scope_gap(graph, err, task).await,
        }
    }

    /// Backwards-compat wrapper: converts a [`VerifyIssue`] to a
    /// [`GraphError`] via `from_verify_issue` and dispatches.
    pub async fn repair(
        &self,
        graph: &Graph,
        issue: &VerifyIssue,
        task: &str,
    ) -> Result<GraphPatch> {
        let err = GraphError::from_verify_issue(issue);
        self.repair_from_error(graph, &err, task).await
    }

    // -----------------------------------------------------------------------
    // L0Structural path — read L2, propose small L0 patch
    // -----------------------------------------------------------------------

    async fn repair_l0_structural(
        &self,
        graph: &Graph,
        err: &GraphError,
        task: &str,
    ) -> Result<GraphPatch> {
        let scope = err.related_nodes();
        let (sub, neighborhood_ids) = subgraph_for_scope(graph, &scope, self.neighborhood_depth);
        let sub_rendered = render_subgraph(&sub);
        let issue_rendered = format_error_for_prompt(err);

        let user_prompt = format!(
            "## Task\n{task}\n\n## Issue (L0 structural)\n{issue_rendered}\n\n## Local subgraph\n{sub_rendered}\n\n\
             Propose a GraphPatch that fixes EXACTLY this issue. Touch only nodes that are \
             listed in the issue's scope or that you are adding. Keep the patch minimal — \
             one to three nodes/edges is normal, more is suspicious.\n\n\
             Output ONE JSON object only:\n\n\
             {{\n  \"patch\": {{\n    \"add_nodes\": [...],\n    \"add_edges\": [...],\n    \
             \"remove_node_ids\": [...],\n    \"remove_edge_indices\": [...],\n    \
             \"reason\": \"<one sentence>\"\n  }},\n  \"rationale\": \"<why this fix, why now>\"\n}}\n\n\
             Patch field schemas:\n\
             - add_nodes:  [{{\"id\":str, \"kind\":str, \"path\":str, \"summary\":str}}]\n\
             - add_edges:  [{{\"source\":str, \"target\":str, \"relation\":str, \"confidence\":0..1, \"evidence\":str}}]\n\
             - remove_node_ids:     [str]\n\
             - remove_edge_indices: [int]  // indices into the LOCAL subgraph above\n\n\
             NodeKind:     File | Function | Class | Module | Config | Task | Other\n\
             RelationType: Contains | BelongsTo | Imports | Exports | DependsOn |\n\
                            Calls | Triggers | Reads | Writes | RevealedBy | InvalidatedBy | Other"
        );

        let system = load_prompt_file("skills/prompts/l0-repairer.md", SYSTEM_PROMPT_L0_REPAIRER);
        let resp = self.call_model(&system, &user_prompt).await?;
        // Reasoning-model fallback (DeepSeek / M3). db2d993d regression.
        let value = parse_json(resp.text_or_reasoning())?;
        let patch_v = value.get("patch").ok_or_else(|| {
            HarnessError::model("repairer: L0 response missing 'patch' field".to_string())
        })?;
        let mut patch = parse_patch(patch_v)?;
        patch.remove_edge_indices =
            translate_local_edge_indices(&sub, graph, &patch.remove_edge_indices);
        validate_scope(graph, &scope, &neighborhood_ids, &patch)?;
        debug!(
            scope_size = scope.len(),
            add_nodes = patch.add_nodes.len(),
            add_edges = patch.add_edges.len(),
            "L0 repair patch produced"
        );
        Ok(patch)
    }

    // -----------------------------------------------------------------------
    // L1Semantic path — re-derive L1 for the drifted node
    // -----------------------------------------------------------------------

    async fn repair_l1_semantic(
        &self,
        graph: &Graph,
        node: &NodeId,
        task: &str,
    ) -> Result<GraphPatch> {
        let enricher = self.l1_enricher.as_ref().ok_or_else(|| {
            HarnessError::model(
                "repairer: L1Semantic repair requires a configured L1Enricher — use \
                 LocalRepairer::with_l1_enricher() to attach one"
                    .to_string(),
            )
        })?;
        let new_l1 = enricher
            .enrich_node(graph, node, task)
            .await
            .map_err(|e| HarnessError::model(format!("L1 re-enrichment failed: {e}")))?;
        debug!(
            node = %node,
            confidence = new_l1.confidence,
            "L1 repair produced new description"
        );
        Ok(GraphPatch {
            set_l1: vec![(node.clone(), new_l1)],
            reason: format!("re-enrich L1 for {node} (semantic drift)"),
            ..Default::default()
        })
    }

    // -----------------------------------------------------------------------
    // ScopeGap path — ask model to propose new nodes+edges filling the gap
    // -----------------------------------------------------------------------

    async fn repair_scope_gap(
        &self,
        graph: &Graph,
        err: &GraphError,
        task: &str,
    ) -> Result<GraphPatch> {
        let (missing_nodes, missing_edges_hint, detail) = match err {
            GraphError::ScopeGap {
                missing_nodes,
                missing_edges,
                detail,
                ..
            } => (missing_nodes.clone(), missing_edges.clone(), detail.clone()),
            other => {
                return Err(HarnessError::model(format!(
                    "repairer: repair_scope_gap called with non-ScopeGap error: {other:?}"
                )));
            }
        };

        // Include the current graph as a (possibly truncated) context so the
        // model can connect new nodes to existing ones cleanly.
        let graph_sketch = render_subgraph(graph);
        let missing_nodes_block = if missing_nodes.is_empty() {
            "(none declared)".to_string()
        } else {
            missing_nodes
                .iter()
                .map(NodeId::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let missing_edges_block = if missing_edges_hint.is_empty() {
            "(none declared)".to_string()
        } else {
            missing_edges_hint
                .iter()
                .map(|(s, t, r)| format!("{s} -[{r}]-> {t}"))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let user_prompt = format!(
            "## Task\n{task}\n\n## Scope gap detail\n{detail}\n\n\
             ## Nodes flagged as missing\n{missing_nodes_block}\n\n\
             ## Edges flagged as missing\n{missing_edges_block}\n\n\
             ## Current graph (for connectivity)\n{graph_sketch}\n\n\
             Propose a GraphPatch that extends the graph to cover the missing region. \
             ONLY add nodes/edges — do NOT remove anything. Make the new nodes connect to \
             existing nodes wherever the task implies a relationship.\n\n\
             Output ONE JSON object only:\n\n\
             {{\n  \"patch\": {{\n    \"add_nodes\": [...],\n    \"add_edges\": [...],\n    \
             \"reason\": \"<one sentence>\"\n  }},\n  \"rationale\": \"<why this expansion is correct>\"\n}}\n\n\
             NodeKind:     File | Function | Class | Module | Config | Task | Other\n\
             RelationType: Contains | BelongsTo | Imports | Exports | DependsOn |\n\
                            Calls | Triggers | Reads | Writes | RevealedBy | InvalidatedBy | Other"
        );

        let system = load_prompt_file("skills/prompts/scope-repairer.md", SYSTEM_PROMPT_SCOPE_REPAIRER);
        let resp = self.call_model(&system, &user_prompt).await?;
        // Reasoning-model fallback (DeepSeek / M3). db2d993d regression.
        let value = parse_json(resp.text_or_reasoning())?;
        let patch_v = value.get("patch").ok_or_else(|| {
            HarnessError::model("repairer: ScopeGap response missing 'patch' field".to_string())
        })?;
        let mut patch = parse_patch(patch_v)?;
        // ScopeGap expansion must not remove anything — sanitize.
        if !patch.remove_node_ids.is_empty() || !patch.remove_edge_indices.is_empty() {
            warn!(
                removes_nodes = patch.remove_node_ids.len(),
                removes_edges = patch.remove_edge_indices.len(),
                "repairer: ScopeGap response tried to remove; ignoring removals"
            );
            patch.remove_node_ids.clear();
            patch.remove_edge_indices.clear();
        }
        debug!(
            add_nodes = patch.add_nodes.len(),
            add_edges = patch.add_edges.len(),
            "ScopeGap expansion patch produced"
        );
        Ok(patch)
    }

    // -----------------------------------------------------------------------
    // Helper — shared model call
    // -----------------------------------------------------------------------

    async fn call_model(
        &self,
        system: &str,
        user: &str,
    ) -> Result<crate::model::ModelResponse> {
        let req = ModelRequest {
            messages: vec![Message::system(system), Message::user(user)],
            tools: vec![],
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            stop: vec![],
        };
        let resp = self.model.complete(req).await?;
        debug!(
            content_len = resp.content.len(),
            reasoning_len = resp.reasoning_content.as_deref().map(str::len).unwrap_or(0),
            tokens = resp.usage.total_tokens,
            "repairer model response"
        );
        Ok(resp)
    }
}

// ---------------------------------------------------------------------------
// Subgraph + scope helpers (work over a Vec<NodeId> rather than VerifyIssue
// so both repair paths reuse them)
// ---------------------------------------------------------------------------

fn subgraph_for_scope(
    graph: &Graph,
    scope: &[NodeId],
    depth: usize,
) -> (Graph, HashSet<NodeId>) {
    if scope.is_empty() {
        let ids: HashSet<NodeId> = graph.nodes.keys().cloned().collect();
        (graph.clone(), ids)
    } else {
        let sub = graph.local_subgraph(scope, depth);
        let ids: HashSet<NodeId> = sub.nodes.keys().cloned().collect();
        (sub, ids)
    }
}

fn validate_scope(
    graph: &Graph,
    scope: &[NodeId],
    neighborhood: &HashSet<NodeId>,
    patch: &GraphPatch,
) -> Result<()> {
    let mut allowed: HashSet<NodeId> = scope.iter().cloned().collect();
    allowed.extend(neighborhood.iter().cloned());
    for n in &patch.add_nodes {
        allowed.insert(n.id.clone());
    }

    for e in &patch.add_edges {
        if !allowed.contains(&e.source) {
            return Err(HarnessError::model(format!(
                "repairer: patch overreaches — add_edge source {} outside issue scope",
                e.source
            )));
        }
        if !allowed.contains(&e.target) {
            return Err(HarnessError::model(format!(
                "repairer: patch overreaches — add_edge target {} outside issue scope",
                e.target
            )));
        }
    }

    let scope_set: HashSet<&NodeId> = scope.iter().collect();
    for id in &patch.remove_node_ids {
        if !scope_set.contains(id) {
            return Err(HarnessError::model(format!(
                "repairer: patch overreaches — remove_node_ids {} outside issue scope",
                id
            )));
        }
    }

    for &idx in &patch.remove_edge_indices {
        let edge = graph.get_edge(idx).ok_or_else(|| {
            HarnessError::model(format!(
                "repairer: remove_edge_indices points to non-existent edge {idx}"
            ))
        })?;
        if !allowed.contains(&edge.source) || !allowed.contains(&edge.target) {
            return Err(HarnessError::model(format!(
                "repairer: patch overreaches — remove_edge {} touches nodes outside scope",
                idx
            )));
        }
    }

    Ok(())
}

fn translate_local_edge_indices(
    sub: &Graph,
    full: &Graph,
    local_indices: &[usize],
) -> Vec<usize> {
    let mut out = Vec::with_capacity(local_indices.len());
    for &i in local_indices {
        let Some(local_edge) = sub.get_edge(i) else {
            continue;
        };
        for (j, e) in full.iter_edges().enumerate() {
            if e.source == local_edge.source
                && e.target == local_edge.target
                && e.relation == local_edge.relation
            {
                out.push(j);
                break;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

fn parse_json(text: &str) -> Result<serde_json::Value> {
    let cleaned = extract_json_block(text)?;
    serde_json::from_str(&cleaned).map_err(|e| {
        HarnessError::model(format!(
            "repairer: invalid JSON: {e}\n--- raw ---\n{text}\n--- cleaned ---\n{cleaned}"
        ))
    })
}

fn parse_patch(v: &serde_json::Value) -> Result<GraphPatch> {
    let obj = v
        .as_object()
        .ok_or_else(|| HarnessError::model("repairer: patch must be an object".to_string()))?;

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
        .ok_or_else(|| HarnessError::model("repairer: node missing 'id'".to_string()))?;
    let kind_str = v.get("kind").and_then(|v| v.as_str()).unwrap_or("Other");
    let kind = parse_node_kind(kind_str);
    let path = v.get("path").and_then(|v| v.as_str()).unwrap_or(id).to_string();
    let summary = v
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(Node::new(id, kind, path, summary))
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
        .ok_or_else(|| HarnessError::model("repairer: edge missing 'source'".to_string()))?;
    let target = v
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::model("repairer: edge missing 'target'".to_string()))?;
    let relation_str = v
        .get("relation")
        .and_then(|v| v.as_str())
        .unwrap_or("Other");
    let relation = parse_relation_type(relation_str);
    let confidence = v
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.6);
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
// Rendering helpers
// ---------------------------------------------------------------------------

fn render_subgraph(g: &Graph) -> String {
    let mut s = String::new();
    s.push_str(&format!("nodes={} edges={}\n", g.node_count(), g.edge_count()));
    let mut ids: Vec<&NodeId> = g.nodes.keys().collect();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    if !ids.is_empty() {
        s.push_str("nodes:\n");
        for id in ids {
            if let Some(n) = g.get_node(id) {
                s.push_str(&format!("  - {} [{:?}] {}\n", n.id, n.kind, n.summary));
            }
        }
    }
    if g.edge_count() > 0 {
        s.push_str("edges (local indices):\n");
        for (i, e) in g.iter_edges().enumerate() {
            s.push_str(&format!(
                "  [{i}] {} -[{:?} c={:.2}]-> {}  evidence={:?}\n",
                e.source, e.relation, e.confidence, e.target, e.evidence
            ));
        }
    }
    s
}

fn format_error_for_prompt(err: &GraphError) -> String {
    let kind = err.kind_label();
    let scope = err.related_nodes();
    let scope_str = if scope.is_empty() {
        "(graph-wide)".to_string()
    } else {
        scope
            .iter()
            .map(NodeId::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let discovered = err.discovered_by().unwrap_or("(unknown)");
    format!(
        "kind: {kind}\nscope: {scope_str}\ndiscovered_by: {discovered}\ndetail: {}",
        err.detail()
    )
}

/// Try to load a prompt from a file, falling back to the hardcoded default.
/// This lets users edit `skills/prompts/repairer-*.md` without recompiling.
fn load_prompt_file(path: &str, default: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| default.to_string())
}

const SYSTEM_PROMPT_L0_REPAIRER: &str = "You are an L0-structure repairer in a graph-centric agent harness. \
Your job is the NARROW operation of fixing ONE flagged L0 issue by proposing a small, local \
GraphPatch. You never expand scope, never refactor things outside the issue's stated scope, \
never bundle unrelated improvements. You output exactly one JSON object — no markdown, no prose. \
If you cannot fix the issue with a local patch, output a patch that partially fixes what you can \
and explain in `reason` that more repair rounds are needed.";

const SYSTEM_PROMPT_SCOPE_REPAIRER: &str = "You are a scope-expander in a graph-centric agent harness. \
Your job is to extend the graph with the missing region described by the issue — propose nodes \
and edges that fill the gap. You ONLY add; you NEVER remove. You connect new nodes to existing \
nodes wherever the task implies. You output exactly one JSON object — no markdown, no prose.";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::verifier::Severity;
    use crate::graph::{Edge, Graph, Node, RelationType};
    use crate::model::{FinishReason, ModelResponse, Usage};
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct MockModel {
        response: Mutex<Option<String>>,
    }

    impl MockModel {
        fn new(response: &str) -> Self {
            Self {
                response: Mutex::new(Some(response.to_string())),
            }
        }
    }

    #[async_trait]
    impl Model for MockModel {
        fn name(&self) -> &str {
            "mock-repairer"
        }
        async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse> {
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
                reasoning_content: None,
                usage: Usage::default(),
            })
        }
    }

    fn three_node_graph() -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::file("a", "module A"));
        g.add_node(Node::file("b", "module B"));
        g.add_node(Node::file("c", "module C"));
        g.add_edge(Edge::new("a", "b", RelationType::Imports, 0.7, "")).unwrap();
        g
    }

    fn missing_edge_issue() -> VerifyIssue {
        VerifyIssue::from_model(
            Severity::High,
            "edge b -> c is missing",
            vec![NodeId::from("b"), NodeId::from("c")],
        )
    }

    #[tokio::test]
    async fn repairer_returns_patch_within_scope() {
        let resp = r#"{
          "patch":{
            "add_nodes":[],
            "add_edges":[{"source":"b","target":"c","relation":"Calls","confidence":0.85,"evidence":"observed in logs"}],
            "remove_node_ids":[],
            "remove_edge_indices":[],
            "reason":"add missing call"
          },
          "rationale":"issue says edge b->c is missing"
        }"#;
        let r = LocalRepairer::new(Arc::new(MockModel::new(resp)));
        let patch = r.repair(&three_node_graph(), &missing_edge_issue(), "test").await.unwrap();
        assert_eq!(patch.add_edges.len(), 1);
        assert_eq!(patch.add_edges[0].source, NodeId::from("b"));
        assert_eq!(patch.add_edges[0].target, NodeId::from("c"));
        assert!(matches!(patch.add_edges[0].relation, RelationType::Calls));
    }

    #[tokio::test]
    async fn rejects_patch_that_overreaches_outside_scope() {
        // Issue scope = {b, c}. Patch tries to add an edge involving 'a',
        // which (with depth=1) IS in the neighborhood since b->a doesn't exist
        // but a->b does, so 'a' enters via the reverse-BFS. To force a real
        // overreach, target a node that's not in graph at all.
        let resp = r#"{
          "patch":{
            "add_edges":[{"source":"b","target":"ghost","relation":"Calls","confidence":0.9,"evidence":""}],
            "reason":"hallucinated edge"
          }
        }"#;
        let r = LocalRepairer::new(Arc::new(MockModel::new(resp)));
        let err = r
            .repair(&three_node_graph(), &missing_edge_issue(), "test")
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("outside issue scope"), "got {msg}");
    }

    #[tokio::test]
    async fn rejects_remove_outside_scope() {
        // Scope = {b, c}. Try to remove node 'a'.
        let resp = r#"{
          "patch":{
            "remove_node_ids":["a"],
            "reason":"unrelated cleanup"
          }
        }"#;
        let r = LocalRepairer::new(Arc::new(MockModel::new(resp)));
        let err = r
            .repair(&three_node_graph(), &missing_edge_issue(), "test")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("outside issue scope"));
    }

    #[tokio::test]
    async fn graph_wide_issue_allows_any_node() {
        // Empty scope means graph-wide — all current nodes are "in scope".
        let mut graph = three_node_graph();
        graph.add_node(Node::file("d", "D"));
        let issue = VerifyIssue::from_model(
            Severity::High,
            "graph-wide: missing structural relation",
            vec![],
        );
        let resp = r#"{
          "patch":{
            "add_edges":[{"source":"a","target":"d","relation":"DependsOn","confidence":0.7,"evidence":""}],
            "reason":"add cross-cutting dep"
          }
        }"#;
        let r = LocalRepairer::new(Arc::new(MockModel::new(resp)));
        let patch = r.repair(&graph, &issue, "test").await.unwrap();
        assert_eq!(patch.add_edges.len(), 1);
    }

    #[tokio::test]
    async fn remove_edge_index_translated_from_local_to_full() {
        // Build a graph where the local subgraph's edge index differs from
        // the full graph's index. We add unrelated edges first so the
        // a->b edge is at index 2 in the full graph.
        let mut g = Graph::new();
        g.add_node(Node::file("x", "X"));
        g.add_node(Node::file("y", "Y"));
        g.add_node(Node::file("a", "A"));
        g.add_node(Node::file("b", "B"));
        // Pad with two unrelated edges that won't appear in the local subgraph
        g.add_edge(Edge::new("x", "y", RelationType::Imports, 0.5, "")).unwrap();
        g.add_edge(Edge::new("y", "x", RelationType::Imports, 0.5, "")).unwrap();
        // Then the edge in scope
        g.add_edge(Edge::new("a", "b", RelationType::Imports, 0.5, "wrong")).unwrap();

        let issue = VerifyIssue::from_model(
            Severity::High,
            "edge a->b is wrong",
            vec![NodeId::from("a"), NodeId::from("b")],
        );
        // The local subgraph extracted around {a,b} contains only a->b at local index 0
        let resp = r#"{
          "patch":{
            "remove_edge_indices":[0],
            "reason":"remove wrong edge"
          }
        }"#;
        let r = LocalRepairer::new(Arc::new(MockModel::new(resp)));
        let patch = r.repair(&g, &issue, "test").await.unwrap();
        // After translation, the index should point to the FULL graph's index = 2
        assert_eq!(patch.remove_edge_indices, vec![2]);
    }

    #[tokio::test]
    async fn rejects_missing_patch_field() {
        let resp = r#"{"rationale":"no patch"}"#;
        let r = LocalRepairer::new(Arc::new(MockModel::new(resp)));
        let err = r
            .repair(&three_node_graph(), &missing_edge_issue(), "test")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("missing 'patch'"));
    }

    #[test]
    fn format_error_for_prompt_handles_empty_scope() {
        let err = GraphError::L0Structural {
            error_type: super::super::graph_loop::L0ErrorType::MissingRelation,
            detail: "edge missing".into(),
            related_nodes: vec![],
            discovered_by: None,
        };
        let s = format_error_for_prompt(&err);
        assert!(s.contains("(graph-wide)"));
        assert!(s.contains("L0Structural"));
        assert!(s.contains("edge missing"));
    }

    #[test]
    fn format_error_for_prompt_joins_scope() {
        let err = GraphError::L0Structural {
            error_type: super::super::graph_loop::L0ErrorType::MissingRelation,
            detail: "concern".into(),
            related_nodes: vec![NodeId::from("x"), NodeId::from("y")],
            discovered_by: Some("batch_2/t3".into()),
        };
        let s = format_error_for_prompt(&err);
        assert!(s.contains("x, y"));
        assert!(s.contains("batch_2/t3"));
    }

    #[tokio::test]
    async fn l1_semantic_repair_without_enricher_errors() {
        let resp = r#"{"patch":{},"rationale":""}"#;
        let r = LocalRepairer::new(Arc::new(MockModel::new(resp)));
        let err = GraphError::L1Semantic {
            node: NodeId::from("a"),
            current_l1: "old".into(),
            actual_l2_evidence: "new evidence".into(),
            discovered_by: None,
        };
        let result = r.repair_from_error(&three_node_graph(), &err, "test").await;
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("requires a configured L1Enricher"));
    }

    #[tokio::test]
    async fn l1_semantic_repair_with_enricher_returns_set_l1_patch() {
        // Repairer's model responds to L0 prompts; L1Enricher's model responds
        // to enrichment prompts. We chain a single MockModel that returns
        // the enrichment JSON when asked.
        let enricher_resp = r#"{
            "responsibility":"freshly derived",
            "implementation":"reads L2",
            "design_intent":"correct drift",
            "constraints":"none",
            "confidence":0.9
        }"#;
        let enricher_model: Arc<dyn Model> = Arc::new(MockModel::new(enricher_resp));
        let mut src = std::collections::HashMap::new();
        src.insert(NodeId::from("a"), "pub fn a() {}\n".into());
        let loader = Arc::new(crate::context::InMemorySources(src));
        let enricher = crate::agent::enricher::L1Enricher::new(enricher_model, loader);
        // Repairer's own model is never called on the L1 path, but we still
        // need it for the struct.
        let unused_repairer_model: Arc<dyn Model> = Arc::new(MockModel::new("{}"));
        let r = LocalRepairer::new(unused_repairer_model).with_l1_enricher(enricher);

        let err = GraphError::L1Semantic {
            node: NodeId::from("a"),
            current_l1: "stale".into(),
            actual_l2_evidence: "drift evidence".into(),
            discovered_by: None,
        };
        let patch = r
            .repair_from_error(&three_node_graph(), &err, "test")
            .await
            .unwrap();
        assert_eq!(patch.set_l1.len(), 1);
        let (id, new_l1) = &patch.set_l1[0];
        assert_eq!(id, &NodeId::from("a"));
        assert_eq!(new_l1.responsibility, "freshly derived");
        // L0 fields untouched
        assert!(patch.add_nodes.is_empty());
        assert!(patch.add_edges.is_empty());
    }

    #[tokio::test]
    async fn scope_gap_repair_returns_expansion_patch() {
        // Model proposes adding two new nodes + one edge connecting one new
        // to an existing node.
        let resp = r#"{
            "patch":{
                "add_nodes":[
                    {"id":"new_x","kind":"File","path":"new_x.rs","summary":"new X"},
                    {"id":"new_y","kind":"File","path":"new_y.rs","summary":"new Y"}
                ],
                "add_edges":[
                    {"source":"a","target":"new_x","relation":"Imports","confidence":0.8,"evidence":"task implies a uses x"}
                ],
                "reason":"fill missing region"
            },
            "rationale":"task requires X and Y"
        }"#;
        let r = LocalRepairer::new(Arc::new(MockModel::new(resp)));
        let err = GraphError::ScopeGap {
            missing_nodes: vec![NodeId::from("new_x"), NodeId::from("new_y")],
            missing_edges: vec![],
            detail: "task needs an X and Y module".into(),
            discovered_by: None,
        };
        let patch = r
            .repair_from_error(&three_node_graph(), &err, "test")
            .await
            .unwrap();
        assert_eq!(patch.add_nodes.len(), 2);
        assert_eq!(patch.add_edges.len(), 1);
        // ScopeGap path forbids removals
        assert!(patch.remove_node_ids.is_empty());
        assert!(patch.remove_edge_indices.is_empty());
    }

    #[tokio::test]
    async fn scope_gap_repair_strips_attempted_removals() {
        // Model misbehaves and tries to remove things; repairer should
        // sanitize the patch.
        let resp = r#"{
            "patch":{
                "add_nodes":[{"id":"z","kind":"File","path":"z.rs","summary":""}],
                "remove_node_ids":["a"],
                "remove_edge_indices":[0],
                "reason":"misbehaving"
            }
        }"#;
        let r = LocalRepairer::new(Arc::new(MockModel::new(resp)));
        let err = GraphError::ScopeGap {
            missing_nodes: vec![NodeId::from("z")],
            missing_edges: vec![],
            detail: "task needs Z".into(),
            discovered_by: None,
        };
        let patch = r
            .repair_from_error(&three_node_graph(), &err, "test")
            .await
            .unwrap();
        assert_eq!(patch.add_nodes.len(), 1);
        assert!(patch.remove_node_ids.is_empty());
        assert!(patch.remove_edge_indices.is_empty());
    }

    #[tokio::test]
    async fn repair_backwards_compat_via_verify_issue_still_works() {
        let resp = r#"{
          "patch":{
            "add_nodes":[],
            "add_edges":[{"source":"b","target":"c","relation":"Calls","confidence":0.85,"evidence":"observed in logs"}],
            "reason":"add missing call"
          }
        }"#;
        let r = LocalRepairer::new(Arc::new(MockModel::new(resp)));
        let patch = r.repair(&three_node_graph(), &missing_edge_issue(), "test").await.unwrap();
        assert_eq!(patch.add_edges.len(), 1);
    }
}
