//! Context builder — distance-layered compression of the world graph.
//!
//! Implements design doc §8: three-layer maps (overview / module-dependency /
//! code-snippets) with distance-based compression. The further a node sits
//! from the task's involved nodes, the more aggressively it is summarized.
//!
//! Phase 1 ships:
//! - [`TokenCounter`] — a heuristic token estimator (no tiktoken dep)
//! - [`ContextBudget`] — the budget allocation table from design doc §8.4
//! - [`ContextBuilder`] — assembles a string context within budget
//!
//! The "Layer 3" code body lookups go through a `SourceLoader` trait so
//! the builder remains domain-agnostic — code, infra HCL, doc snippets, or
//! data-pipeline DDL all plug in the same way.

use crate::error::{HarnessError, Result};
use crate::graph::{Graph, NodeId};
use std::collections::HashMap;
use std::fmt::Write;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// TokenCounter — heuristic estimator
// ---------------------------------------------------------------------------

/// Approximate token estimator used during context budgeting. We deliberately
/// avoid pulling in a tokenizer dependency in Phase 1; the heuristic is
/// "4 characters per token" plus a small overhead. This is accurate within
/// ±15% for English/Chinese-mixed code, which is enough headroom for budget
/// allocation. Replace with a real tokenizer when Phase 2 adds the model
/// integration if precision matters.
#[derive(Debug, Clone, Copy)]
pub struct TokenCounter {
    chars_per_token: f64,
}

impl Default for TokenCounter {
    fn default() -> Self {
        Self { chars_per_token: 4.0 }
    }
}

impl TokenCounter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_ratio(chars_per_token: f64) -> Self {
        Self {
            chars_per_token: chars_per_token.max(0.5),
        }
    }

    pub fn count(&self, text: &str) -> usize {
        let chars = text.chars().count();
        ((chars as f64 / self.chars_per_token).ceil() as usize).max(1)
    }
}

// ---------------------------------------------------------------------------
// Budget allocation per design doc §8.4
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct ContextBudget {
    pub role_def: usize,
    pub overview: usize,
    pub task_def: usize,
    pub local_graph: usize,
    pub prior_results: usize,
    pub l1_section: usize,
    pub code_distance_0: usize,
    pub code_distance_1: usize,
    pub code_distance_2: usize,
    pub reserve: usize,
    pub output: usize,
}

impl ContextBudget {
    /// Default 128K-shaped budget aligned with design doc v2.0 §6.1.
    ///
    /// The L1 section sits between the L0 local graph and the L2 distance
    /// layers — small (~3K) because L1 is condensed semantic descriptions,
    /// not raw content.
    pub fn default_128k() -> Self {
        Self {
            role_def: 3_000,
            overview: 5_000,
            task_def: 5_000,
            local_graph: 12_000,
            l1_section: 3_000,
            prior_results: 15_000,
            code_distance_0: 30_000,
            code_distance_1: 15_000,
            code_distance_2: 10_000,
            reserve: 5_000,
            output: 25_000,
        }
    }

    pub fn input_total(&self) -> usize {
        self.role_def
            + self.overview
            + self.task_def
            + self.local_graph
            + self.l1_section
            + self.prior_results
            + self.code_distance_0
            + self.code_distance_1
            + self.code_distance_2
            + self.reserve
    }

    pub fn total(&self) -> usize {
        self.input_total() + self.output
    }
}

// ---------------------------------------------------------------------------
// SourceLoader — pluggable backend for Layer 3 (actual content)
// ---------------------------------------------------------------------------

/// Abstract source loader: given a node id, return the raw content (e.g.
/// file body, doc body, infra HCL block). Implementations live in
/// `domain` modules so this crate stays domain-agnostic.
pub trait SourceLoader: Send + Sync {
    fn load(&self, node_id: &NodeId) -> Result<String>;
}

/// In-memory loader keyed by node id. Useful for tests and for cached
/// content already in process memory.
pub struct InMemorySources(pub HashMap<NodeId, String>);

impl SourceLoader for InMemorySources {
    fn load(&self, node_id: &NodeId) -> Result<String> {
        self.0
            .get(node_id)
            .cloned()
            .ok_or_else(|| HarnessError::context(format!("no source for {node_id}")))
    }
}

/// Filesystem loader: treats node ids as paths relative to `root`. Used by
/// the demo binary to feed Layer 3 with real file bodies.
pub struct FilesystemSources {
    pub root: PathBuf,
}

impl FilesystemSources {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl SourceLoader for FilesystemSources {
    fn load(&self, node_id: &NodeId) -> Result<String> {
        let path = self.root.join(node_id.as_str());
        std::fs::read_to_string(&path)
            .map_err(|e| HarnessError::context(format!("read {}: {e}", path.display())))
    }
}

/// `SourceLoader` that always reports "no L2 available", with a marker
/// substring (`"no-l2-available"`) the L1Enricher recognises so it can
/// switch to its inferential prompt rather than treating the failure as a
/// real I/O error.
///
/// Use this when the task is **abstract** — planning, requirements, pure
/// architecture sketches — where the nodes simply don't have underlying
/// files or data to read. With this loader, L1 enrichment runs from L0
/// (the node's metadata + neighbors + task description) alone, and the
/// resulting `L1Description.confidence` is capped at 0.6 because it
/// reflects model inference, not direct observation.
pub struct NullSourceLoader;

impl SourceLoader for NullSourceLoader {
    fn load(&self, node_id: &NodeId) -> Result<String> {
        Err(HarnessError::context(format!(
            "no-l2-available: {node_id} has no source/data backing (NullSourceLoader)"
        )))
    }
}

/// Marker substring that load errors include when L2 is *deliberately*
/// unavailable (rather than a transient I/O failure). The L1Enricher
/// treats any failed load as "no L2", but writers of new loaders can
/// include this marker to make logs unambiguous.
pub const NULL_LOADER_MARKER: &str = "no-l2-available";

// ---------------------------------------------------------------------------
// ContextBuilder
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ContextBuilder {
    pub counter: TokenCounter,
    pub budget: ContextBudget,
    /// Max graph distance to include in the local subgraph.
    pub max_graph_depth: usize,
}

impl Default for ContextBuilder {
    fn default() -> Self {
        Self {
            counter: TokenCounter::default(),
            budget: ContextBudget::default_128k(),
            max_graph_depth: 3,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AssembledContext {
    pub text: String,
    pub used_tokens: usize,
    /// Per-section actual token usage for debugging budget overruns.
    pub section_tokens: HashMap<&'static str, usize>,
}

impl ContextBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Assemble the context for a task whose involved nodes are `task_focus`.
    ///
    /// The `loader` provides raw content (Layer 3). The graph supplies the
    /// structural Layer 2 — its node summaries and edges become the local
    /// dependency map.
    pub fn build(
        &self,
        role: &str,
        overview: &str,
        task: &str,
        prior_results: &str,
        graph: &Graph,
        task_focus: &[NodeId],
        loader: &dyn SourceLoader,
    ) -> Result<AssembledContext> {
        let mut sections: HashMap<&'static str, usize> = HashMap::new();
        let mut buf = String::new();

        Self::push_section(
            &mut buf,
            &mut sections,
            "role",
            role,
            self.budget.role_def,
            &self.counter,
        );
        Self::push_section(
            &mut buf,
            &mut sections,
            "overview",
            overview,
            self.budget.overview,
            &self.counter,
        );
        Self::push_section(
            &mut buf,
            &mut sections,
            "task",
            task,
            self.budget.task_def,
            &self.counter,
        );

        let sub = graph.local_subgraph(task_focus, self.max_graph_depth);
        let local_map = render_local_graph(&sub);
        Self::push_section(
            &mut buf,
            &mut sections,
            "local_graph",
            &local_map,
            self.budget.local_graph,
            &self.counter,
        );

        // Layer 3 traversal — computed once, used for both L1 and L2 sections.
        let traversal = graph.bfs_from(task_focus, self.max_graph_depth);
        let mut by_dist: Vec<Vec<NodeId>> = vec![Vec::new(); self.max_graph_depth + 1];
        for (id, &d) in &traversal.distance {
            if d < by_dist.len() {
                by_dist[d].push(id.clone());
            }
        }
        // Deterministic ordering within each distance bucket so prompts are
        // reproducible across runs (HashMap iteration is randomized).
        for bucket in by_dist.iter_mut() {
            bucket.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        }

        // L1: per-node semantic descriptions, layered by graph distance.
        // Full detail at distance 0, brief at 1, oneline at 2+.
        let l1_text = render_l1_by_distance(graph, &by_dist, self.budget.l1_section, &self.counter);
        Self::push_section(
            &mut buf,
            &mut sections,
            "l1",
            &l1_text,
            self.budget.l1_section,
            &self.counter,
        );

        Self::push_section(
            &mut buf,
            &mut sections,
            "prior_results",
            prior_results,
            self.budget.prior_results,
            &self.counter,
        );

        // Layer 3: actual content, sliced by graph distance from the focus.
        let distance_budgets = [
            self.budget.code_distance_0,
            self.budget.code_distance_1,
            self.budget.code_distance_2,
        ];

        for (d, budget) in distance_budgets.iter().enumerate() {
            if d >= by_dist.len() {
                break;
            }
            let mut section = String::new();
            writeln!(section, "## distance {d} entities").ok();
            let mut spent = self.counter.count(&section);
            for id in &by_dist[d] {
                if spent >= *budget {
                    break;
                }
                let raw = match loader.load(id) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let rendered = compress_for_distance(graph, id, &raw, d);
                let cost = self.counter.count(&rendered);
                if spent + cost > *budget {
                    // Try a hard truncation to fit
                    let remaining = budget.saturating_sub(spent);
                    if remaining == 0 {
                        break;
                    }
                    let allowed_chars = (remaining as f64 * 4.0) as usize;
                    let truncated = truncate_chars(&rendered, allowed_chars);
                    let truncated_cost = self.counter.count(&truncated);
                    section.push_str(&truncated);
                    section.push('\n');
                    spent += truncated_cost;
                    break;
                }
                section.push_str(&rendered);
                section.push('\n');
                spent += cost;
            }
            let label: &'static str = match d {
                0 => "code_d0",
                1 => "code_d1",
                _ => "code_d2_plus",
            };
            Self::push_section(&mut buf, &mut sections, label, &section, *budget, &self.counter);
        }

        let used_tokens = self.counter.count(&buf);
        Ok(AssembledContext {
            text: buf,
            used_tokens,
            section_tokens: sections,
        })
    }

    fn push_section(
        buf: &mut String,
        sections: &mut HashMap<&'static str, usize>,
        label: &'static str,
        body: &str,
        budget: usize,
        counter: &TokenCounter,
    ) {
        let cost = counter.count(body);
        let fitted = if cost <= budget {
            body.to_string()
        } else {
            let allowed_chars = (budget as f64 * 4.0) as usize;
            truncate_chars(body, allowed_chars)
        };
        let actual = counter.count(&fitted);
        writeln!(buf, "----- {label} -----").ok();
        buf.push_str(&fitted);
        buf.push_str("\n\n");
        sections.insert(label, actual);
    }
}

// ---------------------------------------------------------------------------
// Compression strategies — per design doc §8.3
// ---------------------------------------------------------------------------

/// Compress a node's raw content to a representation suitable for its
/// distance from the task focus.
///
/// - Distance 0: full content
/// - Distance 1: ~60% — first lines + signature lines + comments
/// - Distance 2: signatures only + one-line summary
/// - Distance 3+: name + relations summary
pub fn compress_for_distance(graph: &Graph, id: &NodeId, raw: &str, distance: usize) -> String {
    let summary = graph
        .get_node(id)
        .map(|n| n.summary.as_str())
        .unwrap_or("");
    match distance {
        0 => format!("### {id} (full)\n{raw}"),
        1 => {
            let head: String = raw.lines().take(40).collect::<Vec<_>>().join("\n");
            let sigs = extract_signature_lines(raw, 20);
            format!("### {id} (compressed)\nsummary: {summary}\n--- head ---\n{head}\n--- signatures ---\n{sigs}")
        }
        2 => {
            let sigs = extract_signature_lines(raw, 10);
            format!("### {id} (signatures)\nsummary: {summary}\n{sigs}")
        }
        _ => {
            // Distance 3+: just the name and an outline of relations
            let rels: Vec<String> = graph
                .outgoing(id)
                .map(|e| format!("{:?}→{}", e.relation, e.target))
                .take(8)
                .collect();
            format!("### {id} (name) — {summary} | {}", rels.join(", "))
        }
    }
}

fn extract_signature_lines(src: &str, max_lines: usize) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in src.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("fn ")
            || trimmed.starts_with("pub fn ")
            || trimmed.starts_with("pub(crate) fn ")
            || trimmed.starts_with("async fn ")
            || trimmed.starts_with("pub async fn ")
            || trimmed.starts_with("struct ")
            || trimmed.starts_with("pub struct ")
            || trimmed.starts_with("enum ")
            || trimmed.starts_with("pub enum ")
            || trimmed.starts_with("trait ")
            || trimmed.starts_with("pub trait ")
            || trimmed.starts_with("impl ")
            || trimmed.starts_with("def ")
            || trimmed.starts_with("class ")
            || trimmed.starts_with("function ")
            || trimmed.starts_with("export ")
            || trimmed.starts_with("import ")
            || trimmed.starts_with("//") && trimmed.len() < 200
        {
            out.push(line);
            if out.len() >= max_lines {
                break;
            }
        }
    }
    out.join("\n")
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut buf: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        buf.push_str("\n…[truncated]");
    }
    buf
}

/// Render a small graph as a textual local-dependency map.
pub fn render_local_graph(g: &Graph) -> String {
    let mut s = String::new();
    writeln!(
        s,
        "nodes: {} edges: {} status: {:?}",
        g.node_count(),
        g.edge_count(),
        g.status
    )
    .ok();
    let mut node_ids: Vec<&NodeId> = g.nodes.keys().collect();
    node_ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    for id in node_ids {
        if let Some(n) = g.get_node(id) {
            writeln!(s, "- {} [{:?}] {}", n.id, n.kind, n.summary).ok();
        }
    }
    for e in g.iter_edges() {
        writeln!(
            s,
            "  {} -[{:?} c={:.2}]→ {}",
            e.source, e.relation, e.confidence, e.target
        )
        .ok();
    }
    s
}

/// Render L1 descriptions for the visited subgraph, sliced by graph distance.
///
/// - **Distance 0**: full L1 (all 4 fields + confidence).
/// - **Distance 1**: brief (responsibility + implementation, one line).
/// - **Distance 2+**: oneline (responsibility only).
///
/// Stops emitting once the running token cost would exceed `budget`. Nodes
/// without an L1 entry are silently skipped — they'll show as
/// "(not yet enriched)" in the graph snapshot the proposer sees, and
/// `L1Enricher` will catch them on the next loop iteration.
pub fn render_l1_by_distance(
    graph: &Graph,
    by_dist: &[Vec<NodeId>],
    budget: usize,
    counter: &TokenCounter,
) -> String {
    let mut s = String::new();
    let mut spent = 0usize;
    for (d, bucket) in by_dist.iter().enumerate() {
        let has_any = bucket
            .iter()
            .any(|id| graph.l1.get(id).is_some_and(|x| !x.is_blank()));
        if !has_any {
            continue;
        }
        let header = format!("## L1 at distance {d}\n");
        let header_cost = counter.count(&header);
        if spent + header_cost > budget {
            break;
        }
        s.push_str(&header);
        spent += header_cost;
        for id in bucket {
            let Some(l1) = graph.l1.get(id) else { continue };
            if l1.is_blank() {
                continue;
            }
            let entry = match d {
                0 => format!(
                    "### {id} (c={:.2})\n{}\n",
                    l1.confidence,
                    l1.render_full().trim_end()
                ),
                1 => format!("- {id} (c={:.2}): {}\n", l1.confidence, l1.render_brief()),
                _ => format!("- {id}: {}\n", l1.render_oneline()),
            };
            let cost = counter.count(&entry);
            if spent + cost > budget {
                // Best-effort partial inclusion: append a truncation marker
                // and stop — preserves what fit, signals we ran out.
                let _ = writeln!(s, "[…L1 section truncated to budget…]");
                spent = budget;
                break;
            }
            s.push_str(&entry);
            spent += cost;
        }
        if spent >= budget {
            break;
        }
    }
    if s.is_empty() {
        s.push_str("(no enriched L1 entries yet)\n");
    }
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, Node, RelationType};

    fn small_world() -> (Graph, InMemorySources) {
        let mut g = Graph::new();
        g.add_node(Node::file("a.rs", "module A — handles requests"));
        g.add_node(Node::file("b.rs", "module B — auth helpers"));
        g.add_node(Node::file("c.rs", "module C — db layer"));
        g.add_edge(Edge::new("a.rs", "b.rs", RelationType::Imports, 1.0, ""))
            .unwrap();
        g.add_edge(Edge::new("b.rs", "c.rs", RelationType::Calls, 0.9, ""))
            .unwrap();

        let mut src = HashMap::new();
        src.insert(
            NodeId::from("a.rs"),
            "fn handle_request() { /* ... */ }\n// 200 lines of code\n".into(),
        );
        src.insert(
            NodeId::from("b.rs"),
            "pub fn verify_token(t: &str) -> bool { true }\n".into(),
        );
        src.insert(
            NodeId::from("c.rs"),
            "pub struct Db; impl Db { pub fn query() {} }\n".into(),
        );
        (g, InMemorySources(src))
    }

    #[test]
    fn token_counter_rough() {
        let c = TokenCounter::default();
        assert!(c.count("hello world") >= 2);
        assert!(c.count("hello world") <= 5);
    }

    #[test]
    fn budget_totals_match_design_doc() {
        let b = ContextBudget::default_128k();
        // v2.0 §6.1 introduced the L1 section; input grew to 103K, output
        // shrank to 25K to keep the total at 128K.
        assert_eq!(b.input_total(), 103_000);
        assert_eq!(b.total(), 128_000);
        // L1 section was added in 2.5.5; sanity-check it has nonzero budget.
        assert!(b.l1_section >= 1_000);
    }

    #[test]
    fn context_builder_includes_all_sections() {
        let (g, loader) = small_world();
        let cb = ContextBuilder::new();
        let ctx = cb
            .build(
                "you are an agent",
                "this project is X",
                "task: refactor auth",
                "no prior",
                &g,
                &[NodeId::from("a.rs")],
                &loader,
            )
            .unwrap();
        assert!(ctx.text.contains("role"));
        assert!(ctx.text.contains("overview"));
        assert!(ctx.text.contains("task"));
        assert!(ctx.text.contains("local_graph"));
        // distance 0 should include the focus node
        assert!(ctx.text.contains("a.rs"));
        assert!(ctx.section_tokens.contains_key("role"));
    }

    #[test]
    fn compression_changes_with_distance() {
        let (g, _) = small_world();
        // Use a realistic-sized body so that stripping non-signature lines
        // actually saves space. On tiny inputs the per-section header alone
        // can make d2 longer than d0, which is a property of the budget
        // headers, not the compression semantics.
        let body = "    let value = compute_something_expensive();\n".repeat(40);
        let raw = format!(
            "pub fn one(arg: i32) -> i32 {{\n{body}    value\n}}\n\npub fn two() {{\n{body}}}\n"
        );
        let d0 = compress_for_distance(&g, &NodeId::from("a.rs"), &raw, 0);
        let d1 = compress_for_distance(&g, &NodeId::from("a.rs"), &raw, 1);
        let d2 = compress_for_distance(&g, &NodeId::from("a.rs"), &raw, 2);
        let d3 = compress_for_distance(&g, &NodeId::from("a.rs"), &raw, 3);
        assert!(d0.len() > d1.len(), "d0={} d1={}", d0.len(), d1.len());
        assert!(d1.len() > d2.len(), "d1={} d2={}", d1.len(), d2.len());
        assert!(d2.len() > d3.len(), "d2={} d3={}", d2.len(), d3.len());
        // d2 keeps signatures
        assert!(d2.contains("pub fn"));
        // d3 keeps name + outline only — no function bodies, no `let`
        assert!(!d3.contains("let value"));
    }

    #[test]
    fn truncate_chars_handles_unicode() {
        // 5 Chinese chars × 3 bytes each = 15 bytes; chars-based truncation
        // must not panic on a UTF-8 boundary.
        let s = "图认知引擎";
        let t = truncate_chars(s, 3);
        assert!(t.starts_with("图认知"));
    }

    #[test]
    fn null_source_loader_always_errors_with_marker() {
        let loader = NullSourceLoader;
        let err = loader.load(&NodeId::from("anything")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains(NULL_LOADER_MARKER), "missing marker in {msg:?}");
        assert!(msg.contains("anything"), "missing node id in {msg:?}");
    }

    #[test]
    fn render_l1_by_distance_empty_graph_emits_placeholder() {
        let counter = TokenCounter::default();
        let by_dist: Vec<Vec<NodeId>> = vec![vec![], vec![], vec![]];
        let g = Graph::new();
        let out = render_l1_by_distance(&g, &by_dist, 1000, &counter);
        assert!(out.contains("no enriched L1"));
    }

    #[test]
    fn render_l1_by_distance_uses_full_at_d0_brief_at_d1_oneline_at_d2() {
        let counter = TokenCounter::default();
        let mut g = Graph::new();
        g.add_node(Node::file("near", "near node"));
        g.add_node(Node::file("mid", "mid node"));
        g.add_node(Node::file("far", "far node"));
        g.l1.set(
            NodeId::from("near"),
            crate::graph::L1Description::new("does near", "wraps near-lib", "centralize", "no panics")
                .with_confidence(0.9),
        );
        g.l1.set(
            NodeId::from("mid"),
            crate::graph::L1Description::new("does mid", "calls helpers", "intent-mid", "constraints-mid")
                .with_confidence(0.8),
        );
        g.l1.set(
            NodeId::from("far"),
            crate::graph::L1Description::new("does far", "impl-far", "intent-far", "constraints-far")
                .with_confidence(0.7),
        );
        let by_dist = vec![
            vec![NodeId::from("near")],
            vec![NodeId::from("mid")],
            vec![NodeId::from("far")],
        ];
        let out = render_l1_by_distance(&g, &by_dist, 5_000, &counter);
        // distance 0 → full L1 (all four field labels appear)
        assert!(out.contains("responsibility: does near"));
        assert!(out.contains("design_intent: centralize"));
        // distance 1 → brief format ("does: ... | how: ...")
        assert!(out.contains("does: does mid | how: calls helpers"));
        // Distance 2 → just the oneline responsibility
        assert!(out.contains("- far: does far"));
        // d2 entry must NOT contain implementation/design_intent labels
        let d2_chunk: &str = out.split("- far:").nth(1).unwrap_or("");
        assert!(!d2_chunk.contains("design_intent"));
    }

    #[test]
    fn render_l1_skips_blank_entries_and_missing_l1() {
        let counter = TokenCounter::default();
        let mut g = Graph::new();
        g.add_node(Node::file("filled", "F"));
        g.add_node(Node::file("blank", "B"));
        g.add_node(Node::file("none", "N"));
        g.l1.set(
            NodeId::from("filled"),
            crate::graph::L1Description::new("F-resp", "", "", ""),
        );
        g.l1.set(NodeId::from("blank"), crate::graph::L1Description::empty());
        // "none" has no L1 entry at all
        let by_dist = vec![vec![
            NodeId::from("filled"),
            NodeId::from("blank"),
            NodeId::from("none"),
        ]];
        let out = render_l1_by_distance(&g, &by_dist, 5_000, &counter);
        assert!(out.contains("F-resp"));
        assert!(!out.contains("blank"));
        assert!(!out.contains("- none"));
    }

    #[test]
    fn render_l1_stops_at_budget_with_truncation_marker() {
        let counter = TokenCounter::default();
        let mut g = Graph::new();
        for i in 0..20 {
            let id = format!("n{i}");
            g.add_node(Node::file(&id, "node"));
            g.l1.set(
                NodeId::from(id.as_str()),
                crate::graph::L1Description::new(
                    "a long-ish responsibility line that uses some tokens",
                    "implementation line here",
                    "intent line here",
                    "constraints here",
                ),
            );
        }
        let by_dist = vec![
            (0..20).map(|i| NodeId::from(format!("n{i}").as_str())).collect(),
        ];
        // Tight budget — should produce some entries plus truncation marker.
        let out = render_l1_by_distance(&g, &by_dist, 200, &counter);
        assert!(out.contains("truncated to budget"));
        // We expect at least one entry to have made it in, but not all 20
        let n0_included = out.contains("n0");
        let n19_included = out.contains("n19");
        assert!(n0_included);
        assert!(!n19_included);
    }
}
