//! Code-domain scanner — file discovery, import edges, and symbol extraction.
//!
//! ## Fractal architecture (v2)
//!
//! For every supported source file, the scanner extracts top-level symbols
//! (functions, classes, methods) using language-aware regex patterns. Each
//! symbol becomes a child node of the file node via a `Contains` edge, with
//! `line_start`/`line_end` metadata for granular L2 loading.

use crate::domain::Scanner;
use crate::error::{HarnessError, Result};
use crate::graph::{Edge, Graph, Node, NodeId, NodeKind, RelationType};
use async_trait::async_trait;
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "kt", "rb", "c", "cc", "cpp", "h", "hpp",
    "swift", "scala", "ex", "exs", "lua", "php", "cs",
];

const EXTERNAL_RUST_PREFIXES: &[&str] = &["std::", "core::", "alloc::"];

#[derive(Debug, Clone, Default)]
pub struct CodeScanner {
    /// Skip directories whose name matches any of these. Sensible defaults.
    pub ignore_dirs: Vec<String>,
    /// Optional cap on file count so a huge repo doesn't blow Phase 1 memory.
    pub max_files: Option<usize>,
}

impl CodeScanner {
    pub fn new() -> Self {
        Self {
            ignore_dirs: vec![
                "target".into(),
                "node_modules".into(),
                ".git".into(),
                "dist".into(),
                "build".into(),
                "__pycache__".into(),
                ".venv".into(),
                "venv".into(),
                ".next".into(),
                "out".into(),
                "demo_output".into(),
            ],
            max_files: Some(5_000),
        }
    }

    fn is_supported(p: &Path) -> bool {
        p.extension()
            .and_then(|s| s.to_str())
            .map(|ext| SUPPORTED_EXTENSIONS.contains(&ext))
            .unwrap_or(false)
    }

    fn should_skip_dir(&self, name: &str) -> bool {
        self.ignore_dirs.iter().any(|d| d == name)
    }
}

#[async_trait]
impl Scanner for CodeScanner {
    async fn scan(&self, source: &str) -> Result<Graph> {
        let root = PathBuf::from(source);
        if !root.exists() {
            return Err(HarnessError::scanner(format!(
                "source path does not exist: {source}"
            )));
        }

        let files = self.walk(&root).await?;
        let mut graph = Graph::new();

        // Pass 1: nodes
        for f in &files {
            let rel = relativize(&root, f);
            let size = fs::metadata(f).await.map(|m| m.len()).unwrap_or(0);
            let summary = format!("{rel} ({size} bytes)");
            let mut node = Node::file(rel.clone(), summary);
            if let Some(ext) = f.extension().and_then(|s| s.to_str()) {
                node = node.with_metadata("ext", serde_json::Value::from(ext));
            }
            graph.add_node(node);
        }

        // Pass 2: edges. Dedup at scanner level — each (source, target) pair
        // gets exactly one Imports edge, with evidence accumulating the raw
        // import strings that produced it. (Without dedup, a single
        // `use crate::graph::{A, B, C}` produces 3 edges to the same target.)
        let mut edge_evidence: HashMap<(NodeId, NodeId), Vec<String>> = HashMap::new();

        for f in &files {
            let rel = relativize(&root, f);
            let ext = f.extension().and_then(|s| s.to_str()).unwrap_or("");
            let Ok(source_text) = fs::read_to_string(f).await else {
                continue;
            };
            let source_id = NodeId::from(rel.as_str());
            for raw_target in extract_imports_for_ext(&source_text, ext) {
                if let Some(target) = resolve_target(&rel, &raw_target, ext, &graph) {
                    if target == source_id {
                        continue;
                    }
                    edge_evidence
                        .entry((source_id.clone(), target))
                        .or_default()
                        .push(raw_target);
                }
            }
        }

        for ((source, target), raws) in edge_evidence {
            let n = raws.len();
            let shown = raws.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
            let evidence = if n > 4 {
                format!("{n} imports incl. {shown}")
            } else {
                format!("imports: {shown}")
            };
            let _ = graph.add_edge(Edge::new(
                source,
                target,
                RelationType::Imports,
                0.6,
                evidence,
            ));
        }

        // Pass 3: symbol extraction — Function/Class sub-nodes with Contains edges.
        for f in &files {
            let rel = relativize(&root, f);
            let ext = f.extension().and_then(|s| s.to_str()).unwrap_or("");
            let source_text = match std::fs::read_to_string(f) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let parent_id = NodeId::from(rel.as_str());
            let symbols = extract_symbols_for_ext(&source_text, ext);
            if symbols.is_empty() {
                continue;
            }
            // Mark parent as expanded.
            if let Some(parent) = graph.nodes.get_mut(&parent_id) {
                parent.expanded = true;
            }
            for sym in &symbols {
                let sym_id = NodeId::from(format!("{}:{}", rel, sym.name));
                let kind = match sym.kind {
                    SymbolKind::Function => NodeKind::Function,
                    SymbolKind::Class | SymbolKind::Method => NodeKind::Class,
                };
                let summary = format!("{} (lines {}-{})", sym.name, sym.line_start, sym.line_end);
                let mut node = Node::new(sym_id.clone(), kind, rel.clone(), summary);
                node.metadata
                    .insert("line_start".into(), serde_json::json!(sym.line_start));
                node.metadata
                    .insert("line_end".into(), serde_json::json!(sym.line_end));
                graph.add_node(node);
                graph
                    .add_edge(Edge::new(
                        parent_id.clone(),
                        sym_id,
                        RelationType::Contains,
                        0.9,
                        format!("{} definition", sym.kind.kind_str()),
                    ))
                    .ok();
            }
        }

        // Pass 4: quality metrics — add per-file stats to metadata.
        for f in &files {
            let rel = relativize(&root, f);
            let source_text = match std::fs::read_to_string(f) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let metrics = compute_quality_metrics(&source_text);
            if let Some(node) = graph.nodes.get_mut(&NodeId::from(rel.as_str())) {
                node.metadata.insert("loc".into(), serde_json::json!(metrics.loc));
                node.metadata.insert("unwrap_count".into(), serde_json::json!(metrics.unwrap_count));
                node.metadata.insert("expect_count".into(), serde_json::json!(metrics.expect_count));
                node.metadata.insert("unsafe_count".into(), serde_json::json!(metrics.unsafe_count));
                node.metadata.insert("todo_count".into(), serde_json::json!(metrics.todo_count));
                node.metadata.insert("quality_score".into(), serde_json::json!(metrics.quality_score()));
                if metrics.needs_attention() {
                    node.summary = format!("{} ⚠️", node.summary);
                }
            }
        }

        Ok(graph)
    }
}

impl SymbolKind {
    fn kind_str(&self) -> &str {
        match self {
            Self::Function => "function",
            Self::Class => "class",
            Self::Method => "method",
        }
    }
}

impl CodeScanner {
    async fn walk(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let mut out: Vec<PathBuf> = Vec::new();
        let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];

        while let Some(dir) = stack.pop() {
            let Ok(mut rd) = fs::read_dir(&dir).await else {
                continue;
            };
            while let Ok(Some(entry)) = rd.next_entry().await {
                let path = entry.path();
                let file_name = entry.file_name().to_string_lossy().to_string();
                let Ok(meta) = entry.metadata().await else {
                    continue;
                };
                if meta.is_dir() {
                    if !self.should_skip_dir(&file_name) {
                        stack.push(path);
                    }
                } else if meta.is_file() && Self::is_supported(&path) {
                    out.push(path);
                    if let Some(cap) = self.max_files {
                        if out.len() >= cap {
                            return Ok(out);
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

fn relativize(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| file.to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

/// Dispatch import extraction based on file extension.
pub fn extract_imports_for_ext(text: &str, ext: &str) -> Vec<String> {
    match ext {
        "rs" => extract_rust_uses(text),
        _ => extract_generic_imports(text),
    }
}

/// Parse Rust `use` statements, including brace groups and multi-line
/// statements. Returns the full qualified path of each imported item
/// (e.g. `crate::graph::Graph` or `super::Edge`).
pub fn extract_rust_uses(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buffer = String::new();
    let mut in_use = false;

    for line in text.lines().take(1000) {
        let t = strip_line_comment(line.trim_start());
        if in_use {
            buffer.push(' ');
            buffer.push_str(t);
            if t.contains(';') {
                process_use_statement(&buffer, &mut out);
                buffer.clear();
                in_use = false;
            }
        } else if let Some(rest) = t.strip_prefix("use ") {
            if rest.contains(';') {
                process_use_statement(rest, &mut out);
            } else {
                buffer.push_str(rest);
                in_use = true;
            }
        }
    }
    if in_use && !buffer.is_empty() {
        process_use_statement(&buffer, &mut out);
    }
    out
}

fn strip_line_comment(s: &str) -> &str {
    if let Some(idx) = s.find("//") {
        s[..idx].trim_end()
    } else {
        s
    }
}

fn process_use_statement(stmt: &str, out: &mut Vec<String>) {
    let stmt = stmt.split(';').next().unwrap_or(stmt).trim();
    if stmt.is_empty() {
        return;
    }

    if let Some(brace_idx) = stmt.find('{') {
        let prefix = stmt[..brace_idx].trim().trim_end_matches("::").trim();
        let after_brace = &stmt[brace_idx + 1..];
        let inner = after_brace.split('}').next().unwrap_or(after_brace);
        for raw_item in inner.split(',') {
            let item = raw_item
                .split(" as ")
                .next()
                .unwrap_or(raw_item)
                .trim();
            if item.is_empty() {
                continue;
            }
            if item == "self" {
                if !prefix.is_empty() {
                    out.push(prefix.to_string());
                }
                continue;
            }
            let full = if prefix.is_empty() {
                item.to_string()
            } else {
                format!("{prefix}::{item}")
            };
            // Recurse: an item might itself be `subprefix::{x, y}`
            if full.contains('{') {
                process_use_statement(&full, out);
            } else {
                out.push(full);
            }
        }
    } else {
        let path = stmt.split(" as ").next().unwrap_or(stmt).trim().to_string();
        if !path.is_empty() {
            out.push(path);
        }
    }
}

/// Generic line scanner for non-Rust languages.
pub fn extract_generic_imports(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines().take(500) {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("import ") {
            if let Some(from_idx) = rest.find(" from ") {
                let after = &rest[from_idx + 6..];
                let cleaned = after.trim_matches(&[' ', '"', '\'', ';'][..]);
                if !cleaned.is_empty() {
                    out.push(cleaned.to_string());
                }
            } else {
                let cleaned = rest.trim_matches(&[' ', '"', '\'', ';'][..]);
                if !cleaned.is_empty() {
                    out.push(cleaned.split_whitespace().next().unwrap_or("").to_string());
                }
            }
        } else if let Some(rest) = t.strip_prefix("from ") {
            if let Some(end) = rest.find(" import") {
                let cleaned = rest[..end].trim();
                if !cleaned.is_empty() {
                    out.push(cleaned.to_string());
                }
            }
        } else if let Some(rest) = t.strip_prefix("require(") {
            if let Some(end) = rest.find(')') {
                let cleaned = rest[..end].trim_matches(&[' ', '"', '\''][..]);
                if !cleaned.is_empty() {
                    out.push(cleaned.to_string());
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Symbol extraction (v2: fractal architecture)
// ---------------------------------------------------------------------------

/// A top-level symbol extracted from a source file.
#[derive(Debug, Clone, PartialEq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub line_start: usize,
    pub line_end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SymbolKind {
    Function,
    Class,
    Method,
}

/// Dispatch symbol extraction by extension.
pub fn extract_symbols_for_ext(text: &str, ext: &str) -> Vec<Symbol> {
    match ext {
        "rs" => extract_rust_symbols(text),
        "py" => extract_python_symbols(text),
        "js" | "ts" | "jsx" | "tsx" => extract_js_ts_symbols(text),
        "go" => extract_go_symbols(text),
        "java" | "kt" | "scala" => extract_java_style_symbols(text),
        _ => Vec::new(), // Other languages: only file-level for now
    }
}

/// Rust: fn, pub fn, async fn, impl blocks, struct, enum, trait
fn extract_rust_symbols(text: &str) -> Vec<Symbol> {
    let re = Regex::new(
        r"(?m)^\s*(?:pub(?:\s*\(\s*crate\s*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)"
    ).unwrap();
    let struct_re = Regex::new(r"(?m)^\s*(?:pub\s+)?struct\s+(\w+)").unwrap();
    let impl_re = Regex::new(r"(?m)^\s*impl\s+(?:[\w:<>,& ]+\s+)?(?:for\s+)?(\w+)").unwrap();

    let regions = find_symbol_regions(text);
    let mut symbols = Vec::new();

    for (re_name, kind) in &[
        (&re, SymbolKind::Function),
        (&struct_re, SymbolKind::Class),
        (&impl_re, SymbolKind::Class),
    ] {
        for cap in re_name.captures_iter(text) {
            if let Some(name) = cap.get(1) {
                let name_str = name.as_str().to_string();
                let line = text[..name.start()].lines().count() + 1;
                let (start, end) = find_region(line, &regions);
                symbols.push(Symbol { name: name_str, kind: kind.clone(), line_start: start, line_end: end });
            }
        }
    }
    symbols.dedup_by(|a, b| a.name == b.name && a.line_start == b.line_start);
    symbols
}

/// Python: def and class
fn extract_python_symbols(text: &str) -> Vec<Symbol> {
    let re = Regex::new(r"(?m)^\s*(?:async\s+)?def\s+(\w+)").unwrap();
    let class_re = Regex::new(r"(?m)^\s*class\s+(\w+)").unwrap();
    let regions = find_symbol_regions(text);
    let mut symbols = Vec::new();
    for (r, kind) in &[(re, SymbolKind::Function), (class_re, SymbolKind::Class)] {
        for cap in r.captures_iter(text) {
            if let Some(name) = cap.get(1) {
                let name_str = name.as_str().to_string();
                let line = text[..name.start()].lines().count() + 1;
                let (start, end) = find_region(line, &regions);
                symbols.push(Symbol { name: name_str, kind: kind.clone(), line_start: start, line_end: end });
            }
        }
    }
    symbols.dedup_by(|a, b| a.name == b.name && a.line_start == b.line_start);
    symbols
}

/// JS/TS: function, class, arrow functions, methods
fn extract_js_ts_symbols(text: &str) -> Vec<Symbol> {
    let re = Regex::new(
        r"(?m)^\s*(?:export\s+)?(?:async\s+)?(?:function\s+(\w+)|(?:const|let|var)\s+(\w+)\s*=\s*(?:async\s+)?\(|class\s+(\w+))"
    ).unwrap();
    let regions = find_symbol_regions(text);
    let mut symbols = Vec::new();
    for cap in re.captures_iter(text) {
        let name = cap.get(1).or_else(|| cap.get(2)).or_else(|| cap.get(3));
        if let Some(name) = name {
            let name_str = name.as_str().to_string();
            let line = text[..name.start()].lines().count() + 1;
            let kind = if cap.get(3).is_some() { SymbolKind::Class } else { SymbolKind::Function };
            let (start, end) = find_region(line, &regions);
            symbols.push(Symbol { name: name_str, kind, line_start: start, line_end: end });
        }
    }
    symbols.dedup_by(|a, b| a.name == b.name && a.line_start == b.line_start);
    symbols
}

/// Go: func
fn extract_go_symbols(text: &str) -> Vec<Symbol> {
    let re = Regex::new(r"(?m)^func\s+(?:\(\s*\w+\s+\*?\w+\s*\)\s+)?(\w+)").unwrap();
    let regions = find_symbol_regions(text);
    let mut symbols = Vec::new();
    for cap in re.captures_iter(text) {
        if let Some(name) = cap.get(1) {
            let name_str = name.as_str().to_string();
            let line = text[..name.start()].lines().count() + 1;
            let (start, end) = find_region(line, &regions);
            symbols.push(Symbol { name: name_str, kind: SymbolKind::Function, line_start: start, line_end: end });
        }
    }
    symbols
}

/// Java/Kotlin/Scala: class, interface, fun, def
fn extract_java_style_symbols(text: &str) -> Vec<Symbol> {
    let re = Regex::new(
        r"(?m)^\s*(?:public\s+|private\s+|protected\s+)?(?:static\s+)?(?:[\w<>\[\],\s]+\s+)?(\w+)\s*\([^)]*\)\s*(?:\{|throws)"
    ).unwrap();
    let class_re = Regex::new(r"(?m)^\s*(?:public\s+)?(?:class|interface|object|enum)\s+(\w+)").unwrap();
    let regions = find_symbol_regions(text);
    let mut symbols = Vec::new();
    for (r, kind) in &[(class_re, SymbolKind::Class), (re, SymbolKind::Function)] {
        for cap in r.captures_iter(text) {
            if let Some(name) = cap.get(1) {
                let name_str = name.as_str().to_string();
                let line = text[..name.start()].lines().count() + 1;
                let (start, end) = find_region(line, &regions);
                symbols.push(Symbol { name: name_str, kind: kind.clone(), line_start: start, line_end: end });
            }
        }
    }
    symbols.dedup_by(|a, b| a.name == b.name && a.line_start == b.line_start);
    symbols
}

/// Braces-based region detection: for each line number, find the enclosing
/// `{...}` scope using a simple brace-counting algorithm.
fn find_symbol_regions(text: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut stack: Vec<usize> = Vec::new();
    let mut regions: Vec<(usize, usize)> = Vec::new();

    let mut depth = 0;
    for (i, line) in lines.iter().enumerate() {
        let open = line.chars().filter(|&c| c == '{' || c == '(' || c == '[').count();
        let close = line.chars().filter(|&c| c == '}' || c == ')' || c == ']').count();
        if depth == 0 && open > 0 {
            stack.push(i + 1); // 1-based line number
        }
        depth = (depth + open).saturating_sub(close);
        if depth == 0 && !stack.is_empty() {
            regions.push((stack.pop().unwrap(), i + 1));
            // Also close all pending scopes at depth 0
            while !stack.is_empty() {
                let s = stack.pop().unwrap();
                regions.push((s, i + 1));
            }
        }
    }
    // Close any remaining open scopes at end of file
    for s in stack {
        regions.push((s, lines.len()));
    }
    regions.sort_by_key(|(s, _)| *s);
    regions
}

fn find_region(line: usize, regions: &[(usize, usize)]) -> (usize, usize) {
    regions
        .iter()
        .find(|(s, e)| *s <= line && line <= *e)
        .copied()
        .unwrap_or((line, line + 5))
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

pub fn resolve_target(from: &str, raw: &str, ext: &str, graph: &Graph) -> Option<NodeId> {
    if ext == "rs" {
        if let Some(id) = resolve_rust_target(from, raw, graph) {
            return Some(id);
        }
        // Don't fall back to substring for Rust — too noisy with std:: etc.
        return None;
    }

    let needle = raw.trim_matches(&['"', '\'', ' ', ';', ':', '.'][..]);
    if needle.is_empty() {
        return None;
    }
    graph
        .iter_nodes()
        .find(|n| n.id.as_str().contains(needle))
        .map(|n| n.id.clone())
}

/// Resolve a Rust use-path to an in-graph file node, handling `crate::`,
/// `super::`, `self::`, and bare external-crate prefixes.
pub fn resolve_rust_target(from: &str, raw: &str, graph: &Graph) -> Option<NodeId> {
    let path = raw.trim();
    if path.is_empty() {
        return None;
    }
    if EXTERNAL_RUST_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return None;
    }

    let normalized = if let Some(rest) = path.strip_prefix("crate::") {
        rest.to_string()
    } else if let Some(rest) = path.strip_prefix("self::") {
        let parent = Path::new(from).parent()?.to_string_lossy().into_owned();
        if parent.is_empty() {
            rest.to_string()
        } else {
            format!("{parent}::{rest}")
        }
    } else if path.starts_with("super::") {
        let mut p = path;
        let mut ups = 0;
        while let Some(rest) = p.strip_prefix("super::") {
            ups += 1;
            p = rest;
        }
        // `from = "a/b/c.rs"` represents module `a::b::c`. One `super::`
        // goes to `a::b`, which is the dir containing c.rs. So we apply
        // `ups` parents starting from the file path itself, not its parent.
        let mut dir = Path::new(from).to_path_buf();
        for _ in 0..ups {
            dir = dir.parent()?.to_path_buf();
        }
        let dir_str = dir.to_string_lossy().to_string();
        if dir_str.is_empty() {
            p.to_string()
        } else {
            // dir_str uses '/' already
            format!("{}::{}", dir_str.replace('/', "::"), p)
        }
    } else {
        // External crate (`tokio::fs`, `serde::Serialize`, …) — has no node.
        return None;
    };

    // Convert `::` to `/`, then try increasingly shorter prefixes — the
    // tail of a use path is typically a type name, not a module.
    let normalized_path = normalized.replace("::", "/");
    let parts: Vec<&str> = normalized_path.split('/').collect();
    for prefix_len in (1..=parts.len()).rev() {
        let prefix = parts[..prefix_len].join("/");
        for suffix in [".rs", "/mod.rs"] {
            let cand = format!("{prefix}{suffix}");
            let nid = NodeId::from(cand.as_str());
            if graph.contains_node(&nid) {
                return Some(nid);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Quality metrics — code health indicators for the model to act on.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct QualityMetrics {
    loc: usize,
    unwrap_count: usize,
    expect_count: usize,
    unsafe_count: usize,
    todo_count: usize,
}

impl QualityMetrics {
    /// 0.0 (poor) to 1.0 (clean). Penalizes unwrap-heavy files.
    fn quality_score(&self) -> f64 {
        if self.loc == 0 { return 1.0; }
        let density = (self.unwrap_count + self.unsafe_count) as f64 / self.loc as f64 * 1000.0;
        (1.0 - density.min(1.0)).max(0.0)
    }

    fn needs_attention(&self) -> bool {
        self.quality_score() < 0.7 || self.todo_count > 3 || self.unwrap_count > 5
    }
}

fn compute_quality_metrics(text: &str) -> QualityMetrics {
    let loc = text.lines().count();
    let re_unwrap = regex::Regex::new(r"\.unwrap\s*\(").unwrap();
    let re_expect = regex::Regex::new(r"\.expect\s*\(").unwrap();
    let re_unsafe = regex::Regex::new(r"\bunsafe\b").unwrap();
    let re_todo = regex::Regex::new(r"(?i)\b(TODO|FIXME|HACK|XXX)\b").unwrap();

    QualityMetrics {
        loc,
        unwrap_count: re_unwrap.find_iter(text).count(),
        expect_count: re_expect.find_iter(text).count(),
        unsafe_count: re_unsafe.find_iter(text).count(),
        todo_count: re_todo.find_iter(text).count(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_use_single_line() {
        let imports = extract_rust_uses("use crate::graph::Graph;\nuse std::collections::HashMap;\n");
        assert_eq!(
            imports,
            vec![
                "crate::graph::Graph".to_string(),
                "std::collections::HashMap".to_string()
            ]
        );
    }

    #[test]
    fn rust_use_brace_group_expands() {
        let imports = extract_rust_uses("use crate::graph::{Edge, Node, Graph};\n");
        assert_eq!(
            imports,
            vec![
                "crate::graph::Edge".to_string(),
                "crate::graph::Node".to_string(),
                "crate::graph::Graph".to_string(),
            ]
        );
    }

    #[test]
    fn rust_use_multiline_accumulates() {
        let imports = extract_rust_uses(
            "use crate::{\n    error::Result,\n    graph::Graph,\n};\n",
        );
        assert!(imports.contains(&"crate::error::Result".to_string()));
        assert!(imports.contains(&"crate::graph::Graph".to_string()));
    }

    #[test]
    fn rust_use_self_in_brace() {
        let imports = extract_rust_uses("use crate::graph::{self, Edge};\n");
        assert!(imports.contains(&"crate::graph".to_string()));
        assert!(imports.contains(&"crate::graph::Edge".to_string()));
    }

    #[test]
    fn rust_use_as_alias() {
        let imports = extract_rust_uses("use crate::graph::Graph as G;\n");
        assert_eq!(imports, vec!["crate::graph::Graph".to_string()]);
    }

    #[test]
    fn rust_use_line_comment_stripped() {
        let imports = extract_rust_uses("use crate::graph::Graph; // ignored\n");
        assert_eq!(imports, vec!["crate::graph::Graph".to_string()]);
    }

    #[test]
    fn python_from_import() {
        let imports = extract_generic_imports("from foo.bar import Baz\nimport requests\n");
        assert!(imports.contains(&"foo.bar".to_string()));
        assert!(imports.contains(&"requests".to_string()));
    }

    #[test]
    fn js_import_from() {
        let imports =
            extract_generic_imports("import {x} from \"./util\";\nimport y from 'z';\n");
        assert!(imports.iter().any(|s| s.contains("util")));
        assert!(imports.iter().any(|s| s.contains("z")));
    }

    #[test]
    fn rust_resolve_crate_prefix() {
        let mut g = Graph::new();
        g.add_node(Node::file("graph/mod.rs", "graph mod"));
        g.add_node(Node::file("error.rs", "error mod"));
        assert_eq!(
            resolve_rust_target("main.rs", "crate::graph::Graph", &g),
            Some(NodeId::from("graph/mod.rs"))
        );
        assert_eq!(
            resolve_rust_target("main.rs", "crate::error::Result", &g),
            Some(NodeId::from("error.rs"))
        );
    }

    #[test]
    fn rust_resolve_super_walks_up() {
        let mut g = Graph::new();
        g.add_node(Node::file("graph/mod.rs", "graph mod"));
        g.add_node(Node::file("graph/traversal.rs", "traversal"));
        // From graph/traversal.rs, `super::Graph` means graph::Graph → graph/mod.rs
        assert_eq!(
            resolve_rust_target("graph/traversal.rs", "super::Graph", &g),
            Some(NodeId::from("graph/mod.rs"))
        );
    }

    #[test]
    fn rust_resolve_skips_external_crates() {
        let mut g = Graph::new();
        g.add_node(Node::file("error.rs", "error"));
        assert_eq!(
            resolve_rust_target("error.rs", "std::collections::HashMap", &g),
            None
        );
        // External crates without a leading prefix also skip
        assert_eq!(resolve_rust_target("error.rs", "tokio::fs", &g), None);
    }

    #[test]
    fn generic_resolver_uses_substring() {
        let mut g = Graph::new();
        g.add_node(Node::file("src/util.rs", "util"));
        assert_eq!(
            resolve_target("src/main.py", "util", "py", &g),
            Some(NodeId::from("src/util.rs"))
        );
    }

    #[tokio::test]
    async fn scan_nonexistent_path_errors() {
        let s = CodeScanner::new();
        let r = s.scan("/definitely/not/a/real/path/xyz").await;
        assert!(r.is_err());
    }

    #[test]
    fn extract_rust_symbols_from_real_file() {
        let text = r#"
pub struct GraphLoop {
    pub proposer: GraphProposer,
    pub verifier: Verifier,
}

impl GraphLoop {
    pub fn new(task: impl Into<String>) -> Self {
        Self { /* ... */ }
    }

    pub async fn step(&mut self) -> LoopState {
        // advance one beat
    }

    fn build_final_result(&self) -> FinalResult {
        FinalResult { /* ... */ }
    }
}

async fn handle_task_phase_graph_errors(errors: Vec<GraphError>) -> LoopState {
    // auto-replan
}

pub fn predecessors_of(node: &NodeId) -> Vec<(&Edge, &Node)> {
    // walk inbound edges
}
"#;
        let symbols = extract_rust_symbols(text);
        println!("Extracted {} symbols:", symbols.len());
        for s in &symbols {
            println!("  {} {:?} lines {}-{}", s.name, s.kind, s.line_start, s.line_end);
        }
        assert!(symbols.iter().any(|s| s.name == "GraphLoop" && matches!(s.kind, SymbolKind::Class)));
        assert!(symbols.iter().any(|s| s.name == "step"));
        assert!(symbols.iter().any(|s| s.name == "new"));
        assert!(symbols.iter().any(|s| s.name == "handle_task_phase_graph_errors"));
        assert!(symbols.iter().any(|s| s.name == "predecessors_of"));
    }
}
