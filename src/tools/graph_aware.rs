//! Graph-aware tools — read by `NodeId` instead of by raw `path`.
//!
//! These four tools close the gap between the agent's mental model
//! ("I want to know about the owners-api node") and the file system
//! ("the path is `src/owners/api.rs`"). They:
//!
//! - Translate `NodeId` to file paths via the in-memory [`Graph`]
//! - For L2 reads, route through the agent's scope policy so a
//!   sub-agent can't read outside its `involved_nodes`
//! - Return *graph-shaped* results (snippets with node_id pointers)
//!   so the agent can chain calls without losing context
//!
//! Per the v2 agent-harness spec §2.

use super::{Tool, ToolContext, ToolOutput};
use crate::agent::saturation::jaccard;
use crate::error::{HarnessError, Result};
use crate::graph::{Graph, L1Description, NodeId, NodeKind, RelationType};
use async_trait::async_trait;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Inner state shared by the four graph-aware tools. Held via
/// `Arc<GraphToolsState>` so multiple tool instances can share the
/// same graph handle (and the sub-agent's tool registry doesn't
/// need to clone the entire graph).
#[derive(Debug)]
pub struct GraphToolsState {
    pub graph: Arc<Graph>,
    /// Optional scope — when set, L2 reads outside this set are
    /// denied. The scope is a set of NodeIds whose `path` the agent
    /// is allowed to read. Sub-agents get this populated from
    /// `SubTask.involved_nodes`.
    pub allowed_node_ids: Option<Vec<NodeId>>,
}

impl GraphToolsState {
    pub fn new(graph: Arc<Graph>) -> Self {
        Self {
            graph,
            allowed_node_ids: None,
        }
    }

    pub fn with_scope(mut self, ids: Vec<NodeId>) -> Self {
        self.allowed_node_ids = Some(ids);
        self
    }

    /// True if the agent is allowed to read the file at `path`. The
    /// `allowed_node_ids` is the set of `NodeId`s whose `path` the
    /// agent may touch; reading L2 from a node outside this set is
    /// denied. If no scope is set, all reads are allowed (e.g. main
    /// agent scope).
    fn can_read(&self, node_id: &NodeId) -> bool {
        match &self.allowed_node_ids {
            None => true,
            Some(ids) => ids.iter().any(|allowed| allowed == node_id),
        }
    }
}

#[derive(Clone)]
pub struct GraphAwareTools {
    pub state: Arc<GraphToolsState>,
}

impl GraphAwareTools {
    pub fn new(state: Arc<GraphToolsState>) -> Self {
        Self { state }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…[truncated, total {} chars]", &s[..end], s.len())
    }
}

fn resolve_node_path(node_id: &str, graph: &Graph) -> Result<(NodeId, PathBuf)> {
    let id = NodeId::from(node_id);
    let node = graph.get_node(&id).ok_or_else(|| {
        HarnessError::context(format!("node `{node_id}` not found in graph"))
    })?;
    let path = PathBuf::from(&node.path);
    Ok((id, path))
}

// ---------------------------------------------------------------------------
// Tool 1: read_graph_node
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ReadGraphNodeTool {
    state: Arc<GraphToolsState>,
}

impl ReadGraphNodeTool {
    pub fn new(state: Arc<GraphToolsState>) -> Self {
        Self { state }
    }
}

#[derive(Debug, Deserialize)]
struct ReadGraphNodeArgs {
    node_id: String,
    #[serde(default = "default_layer")]
    layer: String,
    #[serde(default)]
    line_range: Option<(usize, usize)>,
    #[serde(default)]
    depth: Option<usize>,
}

fn default_layer() -> String {
    "L1".to_string()
}

#[async_trait]
impl Tool for ReadGraphNodeTool {
    fn name(&self) -> &str {
        "read_graph_node"
    }

    fn description(&self) -> &str {
        "Read a graph node by NodeId. layer='L0' returns metadata + edges, \
         layer='L1' returns the semantic description, layer='L2' returns the \
         raw file content (with optional line_range). Use this instead of \
         read_file when you know the node's id."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "node_id": {
                    "type": "string",
                    "description": "NodeId in the graph (e.g. 'owners-api', 'src/main.go')"
                },
                "layer": {
                    "type": "string",
                    "enum": ["L0", "L1", "L2"],
                    "description": "Which layer to read. L0 = node + edges, L1 = semantic description, L2 = raw file content.",
                    "default": "L1"
                },
                "line_range": {
                    "type": "array",
                    "items": {"type": "integer"},
                    "minItems": 2,
                    "maxItems": 2,
                    "description": "[start_line, end_line] for L2 reads. 1-based, inclusive."
                },
                "depth": {
                    "type": "integer",
                    "description": "How many hops of neighbor edges to include with L0 (default 0).",
                    "default": 0
                }
            },
            "required": ["node_id"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let started = Instant::now();
        let args: ReadGraphNodeArgs = serde_json::from_value(input)
            .map_err(|e| HarnessError::context(format!("read_graph_node: bad args: {e}")))?;
        let graph = &self.state.graph;
        let (id, path) = resolve_node_path(&args.node_id, graph)?;
        let node = graph.get_node(&id).expect("just resolved");

        let content = match args.layer.as_str() {
            "L0" => render_l0(graph, &id, args.depth.unwrap_or(0)),
            "L1" => render_l1(graph, &id),
            "L2" => {
                if !self.state.can_read(&id) {
                    return Err(HarnessError::context(format!(
                        "read_graph_node: node `{}` is outside the allowed scope; \
                         the sub-agent was only granted access to: {:?}",
                        args.node_id,
                        self.state
                            .allowed_node_ids
                            .as_ref()
                            .map(|v| v.iter().map(|n| n.as_str().to_string()).collect::<Vec<_>>())
                    )));
                }
                render_l2(&path, ctx.cwd.as_path(), args.line_range, ctx.max_output_chars)?
            }
            other => {
                return Err(HarnessError::context(format!(
                    "read_graph_node: unknown layer `{other}` (expected L0|L1|L2)"
                )))
            }
        };

        let body = format!(
            "# node `{}` (kind={:?}) [{}]\n\n{}",
            node.id.as_str(),
            node.kind,
            args.layer,
            content
        );
        let body_len = body.len();
        Ok(ToolOutput {
            content: body,
            structured: Some(serde_json::json!({
                "node_id": node.id.as_str(),
                "kind": node.kind.as_wire(),
                "path": node.path,
                "layer": args.layer,
            })),
            truncated: body_len >= ctx.max_output_chars,
            exit_code: None,
            interrupted: false,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

fn render_l0(graph: &Graph, id: &NodeId, depth: usize) -> String {
    let node = match graph.get_node(id) {
        Some(n) => n,
        None => return format!("(node `{id}` not found)"),
    };
    let mut s = String::new();
    s.push_str(&format!("## Node\n"));
    s.push_str(&format!("- id: `{}`\n", node.id.as_str()));
    s.push_str(&format!("- kind: `{}`\n", node.kind.as_wire()));
    s.push_str(&format!("- path: `{}`\n", node.path));
    s.push_str(&format!("- summary: {}\n", node.summary));
    if !node.metadata.is_empty() {
        s.push_str(&format!("- metadata: {}\n", node.metadata.len()));
    }

    let outs: Vec<_> = graph.outgoing(id).collect();
    let ins: Vec<_> = graph.incoming(id).collect();
    s.push_str(&format!("\n## Edges\n- outgoing: {}\n- incoming: {}\n", outs.len(), ins.len()));
    if depth > 0 {
        s.push_str("\n## Outgoing (depth)\n");
        for e in &outs {
            s.push_str(&format!("- → `{}` ({:?}, conf={:.2})\n", e.target.as_str(), e.relation, e.confidence));
        }
        s.push_str("\n## Incoming (depth)\n");
        for e in &ins {
            s.push_str(&format!("- ← `{}` ({:?}, conf={:.2})\n", e.source.as_str(), e.relation, e.confidence));
        }
    }
    s
}

fn render_l1(graph: &Graph, id: &NodeId) -> String {
    match graph.l1.get(id) {
        Some(l1) => render_l1_text(l1),
        None => {
            // Fall back to the L0 summary when L1 hasn't been
            // populated yet. This is the common case during early
            // Filling before the L1Enricher has caught up.
            match graph.get_node(id) {
                Some(n) => format!(
                    "(L1 not yet enriched for this node)\n\n## L0 summary\n{}",
                    n.summary
                ),
                None => format!("(node `{id}` not found)"),
            }
        }
    }
}

fn render_l1_text(l1: &L1Description) -> String {
    let mut s = String::new();
    s.push_str(&format!("## Responsibility\n{}\n", l1.responsibility));
    if !l1.implementation.is_empty() {
        s.push_str(&format!("\n## Implementation\n{}\n", l1.implementation));
    }
    if !l1.design_intent.is_empty() {
        s.push_str(&format!("\n## Design intent\n{}\n", l1.design_intent));
    }
    if !l1.constraints.is_empty() {
        s.push_str(&format!("\n## Constraints\n{}\n", l1.constraints));
    }
    s
}

fn render_l2(
    path: &Path,
    cwd: &Path,
    line_range: Option<(usize, usize)>,
    max_output: usize,
) -> Result<String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let text = match fs::read_to_string(&abs) {
        Ok(s) => s,
        Err(e) => return Err(HarnessError::context(format!(
            "read_graph_node L2: cannot read `{}`: {e}",
            abs.display()
        ))),
    };
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let body = if let Some((start, end)) = line_range {
        let s = start.saturating_sub(1).min(total);
        let e = end.min(total);
        let slice: Vec<String> = lines[s..e]
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{:>4} | {}", s + i + 1, l))
            .collect();
        slice.join("\n")
    } else {
        let annotated: Vec<String> = lines
            .iter()
            .take(2000)
            .enumerate()
            .map(|(i, l)| format!("{:>4} | {}", i + 1, l))
            .collect();
        if lines.len() > 2000 {
            annotated.join("\n") + &format!("\n…[truncated at line 2000 of {total}]")
        } else {
            annotated.join("\n")
        }
    };
    Ok(truncate(&body, max_output))
}

// ---------------------------------------------------------------------------
// Tool 2: search_graph
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SearchGraphTool {
    state: Arc<GraphToolsState>,
}

impl SearchGraphTool {
    pub fn new(state: Arc<GraphToolsState>) -> Self {
        Self { state }
    }
}

#[derive(Debug, Deserialize)]
struct SearchGraphArgs {
    query: String,
    #[serde(default = "default_search_in")]
    search_in: String,
    #[serde(default)]
    node_kind_filter: Option<Vec<String>>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_search_in() -> String {
    "all".to_string()
}
fn default_limit() -> usize {
    20
}

#[async_trait]
impl Tool for SearchGraphTool {
    fn name(&self) -> &str {
        "search_graph"
    }

    fn description(&self) -> &str {
        "Search the L0/L1 graph for nodes matching a query. Returns up to \
         `limit` results, each with node_id, snippet, and a Jaccard score. \
         Use this instead of grep when you want to find concepts rather \
         than literal strings."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query (Chinese or English)"},
                "search_in": {
                    "type": "string",
                    "enum": ["node_summary", "l1_responsibility", "edge_evidence", "all"],
                    "default": "all"
                },
                "node_kind_filter": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Restrict to specific node kinds (e.g. ['File', 'Task'])"
                },
                "limit": {"type": "integer", "default": 20, "minimum": 1, "maximum": 200}
            },
            "required": ["query"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let started = Instant::now();
        let args: SearchGraphArgs = serde_json::from_value(input)
            .map_err(|e| HarnessError::context(format!("search_graph: bad args: {e}")))?;
        let graph = &self.state.graph;
        let kinds_filter: Option<Vec<NodeKind>> = args.node_kind_filter.as_ref().map(|v| {
            v.iter()
                .map(|s| NodeKind::parse_wire(s))
                .collect()
        });
        let mut hits: Vec<(NodeId, String, f64)> = Vec::new();
        for node in graph.iter_nodes() {
            if let Some(kinds) = &kinds_filter {
                if !kinds.iter().any(|k| k == &node.kind) {
                    continue;
                }
            }
            let l1_text = graph
                .l1
                .get(&node.id)
                .map(|l1| l1.render_oneline())
                .unwrap_or_default();
            let corpus = match args.search_in.as_str() {
                "node_summary" => node.summary.clone(),
                "l1_responsibility" => l1_text,
                "edge_evidence" => {
                    let mut s = String::new();
                    for e in graph.outgoing(&node.id) {
                        s.push_str(&e.evidence);
                        s.push('\n');
                    }
                    for e in graph.incoming(&node.id) {
                        s.push_str(&e.evidence);
                        s.push('\n');
                    }
                    s
                }
                _ => format!("{} {}", node.summary, l1_text),
            };
            let score = jaccard(&args.query, &corpus);
            if score > 0.0 {
                let snippet = if corpus.len() > 100 {
                    format!("{}…", &corpus[..100])
                } else {
                    corpus
                };
                hits.push((node.id.clone(), snippet, score));
            }
        }
        hits.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(args.limit);
        let mut body = format!("Found {} hit(s) for `{}`:\n\n", hits.len(), args.query);
        for (id, snippet, score) in &hits {
            body.push_str(&format!("- `{}` (score={:.3}): {}\n", id.as_str(), score, snippet));
        }
        if hits.is_empty() {
            body.push_str("(no matching nodes — try a different query or relax filters)\n");
        }
        Ok(ToolOutput {
            content: truncate(&body, ctx.max_output_chars),
            structured: Some(serde_json::json!({
                "query": args.query,
                "hit_count": hits.len(),
                "hits": hits.iter().map(|(id, sn, sc)| serde_json::json!({
                    "node_id": id.as_str(), "score": sc, "snippet": sn,
                })).collect::<Vec<_>>(),
            })),
            truncated: body.len() >= ctx.max_output_chars,
            exit_code: None,
            interrupted: false,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

// ---------------------------------------------------------------------------
// Tool 3: find_similar_nodes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FindSimilarNodesTool {
    state: Arc<GraphToolsState>,
}

impl FindSimilarNodesTool {
    pub fn new(state: Arc<GraphToolsState>) -> Self {
        Self { state }
    }
}

#[derive(Debug, Deserialize)]
struct FindSimilarArgs {
    /// Either a node_id (compare to that node's text) or a free-text
    /// `text` field (compare to all nodes).
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default = "default_similarity_to")]
    similarity_to: String,
    #[serde(default = "default_top_k")]
    top_k: usize,
    #[serde(default = "default_similarity_threshold")]
    threshold: f64,
}

fn default_similarity_to() -> String {
    "L0_summary".to_string()
}
fn default_top_k() -> usize {
    5
}
fn default_similarity_threshold() -> f64 {
    0.7
}

#[async_trait]
impl Tool for FindSimilarNodesTool {
    fn name(&self) -> &str {
        "find_similar_nodes"
    }

    fn description(&self) -> &str {
        "Find the top-K most similar nodes in the graph. Use this to detect \
         when the model is asking about the same concept as an existing node, \
         or to check whether a new question duplicates a recent one. Returns \
         (node_id, score) pairs sorted by score."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "node_id": {"type": "string", "description": "Compare this node's text against all others."},
                "text": {"type": "string", "description": "Compare this free text against all nodes."},
                "similarity_to": {
                    "type": "string",
                    "enum": ["L0_summary", "L1_responsibility", "L1_full"],
                    "default": "L0_summary"
                },
                "top_k": {"type": "integer", "default": 5, "minimum": 1, "maximum": 50},
                "threshold": {"type": "number", "default": 0.7, "minimum": 0.0, "maximum": 1.0}
            }
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let started = Instant::now();
        let args: FindSimilarArgs = serde_json::from_value(input)
            .map_err(|e| HarnessError::context(format!("find_similar_nodes: bad args: {e}")))?;
        let graph = &self.state.graph;
        let query = if let Some(t) = args.text {
            t
        } else if let Some(ref nid) = args.node_id {
            let (id, _) = resolve_node_path(nid, graph)?;
            match args.similarity_to.as_str() {
                "L1_responsibility" | "L1_full" => graph
                    .l1
                    .get(&id)
                    .map(|l1| l1.render_oneline())
                    .unwrap_or_default(),
                _ => graph
                    .get_node(&id)
                    .map(|n| n.summary.clone())
                    .unwrap_or_default(),
            }
        } else {
            return Err(HarnessError::context(
                "find_similar_nodes: must provide `node_id` or `text`".to_string(),
            ));
        };
        if query.trim().is_empty() {
            return Err(HarnessError::context(
                "find_similar_nodes: empty query".to_string(),
            ));
        }
        let mut scores: Vec<(NodeId, f64)> = Vec::new();
        for node in graph.iter_nodes() {
            let candidate = match args.similarity_to.as_str() {
                "L1_responsibility" | "L1_full" => graph
                    .l1
                    .get(&node.id)
                    .map(|l1| l1.render_oneline())
                    .unwrap_or_default(),
                _ => node.summary.clone(),
            };
            let score = jaccard(&query, &candidate);
            if score >= args.threshold {
                scores.push((node.id.clone(), score));
            }
        }
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores.truncate(args.top_k);
        let mut body = format!("Top {} similar (threshold >= {:.2}):\n\n", scores.len(), args.threshold);
        for (id, s) in &scores {
            body.push_str(&format!("- `{}` (score={:.3})\n", id.as_str(), s));
        }
        if scores.is_empty() {
            body.push_str("(no nodes above threshold)\n");
        }
        Ok(ToolOutput {
            content: truncate(&body, ctx.max_output_chars),
            structured: Some(serde_json::json!({
                "query_preview": &query[..query.len().min(80)],
                "results": scores.iter().map(|(id, s)| serde_json::json!({
                    "node_id": id.as_str(), "score": s,
                })).collect::<Vec<_>>(),
            })),
            truncated: body.len() >= ctx.max_output_chars,
            exit_code: None,
            interrupted: false,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

// ---------------------------------------------------------------------------
// Tool 4: trace_dependency
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TraceDependencyTool {
    state: Arc<GraphToolsState>,
}

impl TraceDependencyTool {
    pub fn new(state: Arc<GraphToolsState>) -> Self {
        Self { state }
    }
}

#[derive(Debug, Deserialize)]
struct TraceDependencyArgs {
    start: String,
    relation: String,
    #[serde(default = "default_direction")]
    direction: String,
    #[serde(default = "default_max_depth")]
    max_depth: usize,
}

fn default_direction() -> String {
    "downstream".to_string()
}
fn default_max_depth() -> usize {
    5
}

fn parse_relation(s: &str) -> Result<RelationType> {
    Ok(RelationType::parse_wire(s))
}

#[async_trait]
impl Tool for TraceDependencyTool {
    fn name(&self) -> &str {
        "trace_dependency"
    }

    fn description(&self) -> &str {
        "Walk the graph along a specific relation. Returns paths (lists of \
         NodeIds) starting at `start`. Use direction='upstream' to find \
         prerequisites, 'downstream' for dependents, 'both' for the full \
         picture. Useful for impact analysis: 'what breaks if X fails?'"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "start": {"type": "string", "description": "Starting node id"},
                "relation": {
                    "type": "string",
                    "enum": ["DependsOn", "LeadsTo", "Contains", "Imports", "Exports",
                             "Calls", "Triggers", "Reads", "Writes", "BelongsTo"],
                    "description": "Which edge type to follow"
                },
                "direction": {
                    "type": "string",
                    "enum": ["upstream", "downstream", "both"],
                    "default": "downstream"
                },
                "max_depth": {"type": "integer", "default": 5, "minimum": 1, "maximum": 50}
            },
            "required": ["start", "relation"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let started = Instant::now();
        let args: TraceDependencyArgs = serde_json::from_value(input)
            .map_err(|e| HarnessError::context(format!("trace_dependency: bad args: {e}")))?;
        let graph = &self.state.graph;
        let (start_id, _) = resolve_node_path(&args.start, graph)?;
        let rel = parse_relation(&args.relation)?;
        let max_depth = args.max_depth.clamp(1, 50);

        let mut paths: Vec<Vec<NodeId>> = Vec::new();
        let mut current_paths: Vec<Vec<NodeId>> = vec![vec![start_id.clone()]];
        for _ in 0..max_depth {
            let mut next_paths: Vec<Vec<NodeId>> = Vec::new();
            for path in &current_paths {
                let last = path.last().expect("non-empty");
                let nexts: Vec<NodeId> = match args.direction.as_str() {
                    // Per the v2 spec: "upstream" = prerequisites =
                    // what THIS node depends on = its outgoing edges.
                    "upstream" => graph
                        .outgoing(last)
                        .filter(|e| e.relation == rel)
                        .map(|e| e.target.clone())
                        .collect(),
                    // "downstream" = dependents = what depends on
                    // THIS node = its incoming edges.
                    "downstream" => graph
                        .incoming(last)
                        .filter(|e| e.relation == rel)
                        .map(|e| e.source.clone())
                        .collect(),
                    "both" => {
                        let mut v: Vec<NodeId> = graph
                            .outgoing(last)
                            .filter(|e| e.relation == rel)
                            .map(|e| e.target.clone())
                            .collect();
                        v.extend(
                            graph
                                .incoming(last)
                                .filter(|e| e.relation == rel)
                                .map(|e| e.source.clone()),
                        );
                        v
                    }
                    _ => Vec::new(),
                };
                for n in nexts {
                    if path.contains(&n) {
                        continue;
                    } // cycle guard
                    let mut np = path.clone();
                    np.push(n);
                    next_paths.push(np);
                }
            }
            if next_paths.is_empty() {
                break;
            }
            paths.extend(next_paths.iter().cloned());
            current_paths = next_paths;
        }

        let mut body = format!(
            "Trace from `{}` along {:?} ({}):\n\n",
            start_id.as_str(),
            rel,
            args.direction
        );
        if paths.is_empty() {
            body.push_str("(no reachable nodes in this direction within max_depth)\n");
        } else {
            for p in &paths {
                let ids: Vec<String> = p.iter().map(|n| n.as_str().to_string()).collect();
                body.push_str(&format!("- {}\n", ids.join(" → ")));
            }
        }
        Ok(ToolOutput {
            content: truncate(&body, ctx.max_output_chars),
            structured: Some(serde_json::json!({
                "start": start_id.as_str(),
                "relation": rel.as_wire(),
                "direction": args.direction,
                "path_count": paths.len(),
                "paths": paths.iter().map(|p| p.iter().map(|n| n.as_str()).collect::<Vec<_>>()).collect::<Vec<_>>(),
            })),
            truncated: body.len() >= ctx.max_output_chars,
            exit_code: None,
            interrupted: false,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, Node};

    fn fixture_graph() -> Arc<Graph> {
        let mut g = Graph::new();
        g.add_node(Node::task("owners-api", "owner management REST API"));
        g.add_node(Node::task("TodoItem", "data type for a todo row"));
        g.add_node(Node::file("src/owners/api.go", "owners implementation"));
        g.add_node(Node::task("fees-api", "billing and fee service"));
        g.add_edge(Edge::new("owners-api", "TodoItem", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        g.add_edge(
            Edge::new("src/owners/api.go", "owners-api", RelationType::Contains, 1.0, ""),
        )
        .unwrap();
        Arc::new(g)
    }

    fn make_ctx() -> ToolContext {
        ToolContext::new("/tmp")
    }

    #[tokio::test]
    async fn read_graph_node_l1_falls_back_to_summary() {
        let g = fixture_graph();
        let state = Arc::new(GraphToolsState::new(g));
        let tool = ReadGraphNodeTool::new(state);
        let out = tool
            .call(serde_json::json!({"node_id": "owners-api", "layer": "L1"}), &make_ctx())
            .await
            .unwrap();
        assert!(out.content.contains("owner management REST API"));
    }

    #[tokio::test]
    async fn read_graph_node_l0_includes_edges() {
        let g = fixture_graph();
        let state = Arc::new(GraphToolsState::new(g));
        let tool = ReadGraphNodeTool::new(state);
        let out = tool
            .call(serde_json::json!({"node_id": "owners-api", "layer": "L0", "depth": 1}), &make_ctx())
            .await
            .unwrap();
        assert!(out.content.contains("Edges"));
        assert!(out.content.contains("outgoing"));
    }

    #[tokio::test]
    async fn read_graph_node_unknown_layer_errors() {
        let g = fixture_graph();
        let state = Arc::new(GraphToolsState::new(g));
        let tool = ReadGraphNodeTool::new(state);
        let res = tool
            .call(serde_json::json!({"node_id": "owners-api", "layer": "L9"}), &make_ctx())
            .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn read_graph_node_l2_respects_scope() {
        let g = fixture_graph();
        let state = Arc::new(GraphToolsState::new(g).with_scope(vec![NodeId::from("TodoItem")]));
        let tool = ReadGraphNodeTool::new(state);
        // owners-api is in the graph but not in allowed_node_ids
        let res = tool
            .call(serde_json::json!({"node_id": "src/owners/api.go", "layer": "L2"}), &make_ctx())
            .await;
        assert!(res.is_err(), "L2 read should be denied outside scope");
    }

    #[tokio::test]
    async fn search_graph_finds_relevant_nodes() {
        let g = fixture_graph();
        let state = Arc::new(GraphToolsState::new(g));
        let tool = SearchGraphTool::new(state);
        let out = tool
            .call(serde_json::json!({"query": "owner", "limit": 5}), &make_ctx())
            .await
            .unwrap();
        assert!(out.content.contains("owners-api"));
    }

    #[tokio::test]
    async fn search_graph_no_match_returns_empty() {
        let g = fixture_graph();
        let state = Arc::new(GraphToolsState::new(g));
        let tool = SearchGraphTool::new(state);
        // Chinese characters that share no bigrams with the
        // English-only fixture corpus.
        let out = tool
            .call(
                serde_json::json!({"query": "一二三四五六七八九十", "limit": 5}),
                &make_ctx(),
            )
            .await
            .unwrap();
        assert!(
            out.content.contains("no matching nodes"),
            "expected no hits, got: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn find_similar_nodes_finds_duplicates() {
        let g = fixture_graph();
        let state = Arc::new(GraphToolsState::new(g));
        let tool = FindSimilarNodesTool::new(state);
        let out = tool
            .call(
                serde_json::json!({
                    "text": "owner management REST API",
                    "top_k": 3,
                    "threshold": 0.2
                }),
                &make_ctx(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("owners-api"));
    }

    #[tokio::test]
    async fn trace_dependency_finds_prerequisites() {
        let g = fixture_graph();
        let state = Arc::new(GraphToolsState::new(g));
        let tool = TraceDependencyTool::new(state);
        let out = tool
            .call(
                serde_json::json!({
                    "start": "owners-api",
                    "relation": "DependsOn",
                    "direction": "upstream",
                    "max_depth": 3
                }),
                &make_ctx(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("TodoItem"));
    }

    #[tokio::test]
    async fn trace_dependency_no_path_returns_empty() {
        let g = fixture_graph();
        let state = Arc::new(GraphToolsState::new(g));
        let tool = TraceDependencyTool::new(state);
        let out = tool
            .call(
                serde_json::json!({
                    "start": "owners-api",
                    "relation": "Imports",
                    "direction": "downstream",
                    "max_depth": 3
                }),
                &make_ctx(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("no reachable nodes"));
    }
}
