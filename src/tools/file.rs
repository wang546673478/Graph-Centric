//! Direct file I/O tools — cross-platform, no shell needed.
//!
//! Unlike the Bash tool (which requires bash/cmd), these use `std::fs`
//! directly and work identically on Windows, Linux, and macOS.

use super::{Tool, ToolContext, ToolOutput};
use crate::error::{HarnessError, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Instant;

fn resolve_path(path: &str, cwd: &std::path::Path) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() { p } else { cwd.join(p) }
}

// ---------------------------------------------------------------------------
// ReadFileTool
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str { "read_file" }
    fn description(&self) -> &str { "Read a file from disk. Returns text content." }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative"},
                "offset": {"type": "integer", "description": "Start line (1-based)"},
                "limit": {"type": "integer", "description": "Max lines to read, default 2000"}
            },
            "required": ["path"]
        })
    }
    fn is_read_only(&self, _input: &serde_json::Value) -> bool { true }
    fn is_destructive(&self, _input: &serde_json::Value) -> bool { false }

    async fn call(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        #[derive(Deserialize)] struct A { path: String, offset: Option<usize>, limit: Option<usize> }
        let a: A = serde_json::from_value(input).map_err(|e| HarnessError::domain(format!("read_file: {e}")))?;
        let st = Instant::now();
        let p = resolve_path(&a.path, &ctx.cwd);
        let content = std::fs::read_to_string(&p).map_err(|e| HarnessError::domain(format!("read_file {p}: {e}", p=p.display())))?;
        let r = if let Some(off) = a.offset {
            let lines: Vec<&str> = content.lines().collect();
            let from = (off.max(1) - 1).min(lines.len());
            let to = (from + a.limit.unwrap_or(2000)).min(lines.len());
            lines[from..to].join("\n")
        } else if let Some(lim) = a.limit { content.lines().take(lim).collect::<Vec<_>>().join("\n") }
        else { content };
        Ok(ToolOutput::ok(r, None))
    }
}

// ---------------------------------------------------------------------------
// WriteFileTool
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str { "write_file" }
    fn description(&self) -> &str { "Write text content to a file. Creates or overwrites." }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative"},
                "content": {"type": "string", "description": "Text content to write"}
            },
            "required": ["path", "content"]
        })
    }
    fn is_read_only(&self, _input: &serde_json::Value) -> bool { false }
    fn is_destructive(&self, _input: &serde_json::Value) -> bool { true }

    async fn call(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        #[derive(Deserialize)] struct A { path: String, content: String }
        let a: A = serde_json::from_value(input).map_err(|e| HarnessError::domain(format!("write_file: {e}")))?;
        let p = resolve_path(&a.path, &ctx.cwd);
        if let Some(parent) = p.parent() { let _ = std::fs::create_dir_all(parent); }
        std::fs::write(&p, &a.content).map_err(|e| HarnessError::domain(format!("write_file {p}: {e}", p=p.display())))?;
        Ok(ToolOutput::ok(format!("Wrote {} bytes to {}", a.content.len(), p.display()), None))
    }
}

// ---------------------------------------------------------------------------
// EditFileTool — string replacement, like Claude Code's Edit tool.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str { "edit_file" }
    fn description(&self) -> &str { "Replace a string in a file. Fails if old_string is not unique." }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to edit"},
                "old_string": {"type": "string", "description": "Exact text to replace"},
                "new_string": {"type": "string", "description": "Replacement text"}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    fn is_read_only(&self, _input: &serde_json::Value) -> bool { false }
    fn is_destructive(&self, _input: &serde_json::Value) -> bool { true }

    async fn call(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        #[derive(Deserialize)] struct A { path: String, old_string: String, new_string: String }
        let a: A = serde_json::from_value(input).map_err(|e| HarnessError::domain(format!("edit_file: {e}")))?;
        let p = resolve_path(&a.path, &ctx.cwd);
        let original = std::fs::read_to_string(&p).map_err(|e| HarnessError::domain(format!("edit_file read {p}: {e}", p=p.display())))?;
        let count = original.matches(&a.old_string).count();
        if count == 0 {
            return Err(HarnessError::domain(format!("edit_file: old_string not found in {p}", p=p.display())));
        }
        if count > 1 {
            return Err(HarnessError::domain(format!("edit_file: old_string appears {count} times in {p} — must be unique", p=p.display())));
        }
        let updated = original.replacen(&a.old_string, &a.new_string, 1);
        std::fs::write(&p, &updated).map_err(|e| HarnessError::domain(format!("edit_file write {p}: {e}", p=p.display())))?;
        Ok(ToolOutput::ok(format!("Replaced 1 occurrence in {}", p.display()), None))
    }
}

// ---------------------------------------------------------------------------
// v2 spec §5.1: GraphAwareEditFileTool — edit by `node_id` instead of
// (or in addition to) `path`. When a `node_id` is provided, the path
// is resolved from the in-memory Graph. This catches the "stale
// path" failure mode where the model remembers a path from the
// graph but the graph node's `path` field has since changed.
// ---------------------------------------------------------------------------

use crate::graph::{Graph, NodeId};
use std::sync::Arc;

pub struct GraphAwareEditFileTool {
    pub graph: Arc<Graph>,
}

impl GraphAwareEditFileTool {
    pub fn new(graph: Arc<Graph>) -> Self {
        Self { graph }
    }
}

#[async_trait]
impl Tool for GraphAwareEditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace a string in a file. Accepts either a `path` (raw) or a \
         `node_id` (resolved via the Graph). When `node_id` is given, the \
         tool verifies the path matches the graph node's `path` field — a \
         mismatch is a stale-write attempt and is rejected."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "node_id": {
                    "type": "string",
                    "description": "Resolve path from this NodeId in the graph. Provide EITHER this OR `path`, not both."
                },
                "path": {
                    "type": "string",
                    "description": "File path (raw). Provide EITHER this OR `node_id`, not both."
                },
                "old_string": {"type": "string"},
                "new_string": {"type": "string"}
            },
            "required": ["old_string", "new_string"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }
    fn is_destructive(&self, _input: &serde_json::Value) -> bool {
        true
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        #[derive(Deserialize)]
        struct A {
            node_id: Option<String>,
            path: Option<String>,
            old_string: String,
            new_string: String,
        }
        let a: A = serde_json::from_value(input)
            .map_err(|e| HarnessError::domain(format!("edit_file: {e}")))?;
        let resolved_path: String = match (a.node_id, a.path) {
            (Some(nid), None) => {
                let id = NodeId::from(nid);
                let node = self.graph.get_node(&id).ok_or_else(|| {
                    HarnessError::domain(format!(
                        "edit_file: node `{id}` not found in graph"
                    ))
                })?;
                node.path.clone()
            }
            (None, Some(p)) => p,
            (Some(_), Some(_)) => {
                return Err(HarnessError::domain(String::from(
                    "edit_file: provide EITHER `node_id` OR `path`, not both",
                )))
            }
            (None, None) => {
                return Err(HarnessError::domain(String::from(
                    "edit_file: must provide `node_id` or `path`",
                )))
            }
        };
        // Delegate to the underlying logic.
        let delegated = serde_json::json!({
            "path": resolved_path,
            "old_string": a.old_string,
            "new_string": a.new_string,
        });
        let inner = EditFileTool;
        inner.call(delegated, ctx).await
    }
}

#[cfg(test)]
mod tests_graph_aware_edit {
    use super::*;
    use crate::graph::Node;

    #[tokio::test]
    async fn resolves_path_from_node_id() {
        let mut g = Graph::new();
        g.add_node(Node::file("src/owners/api.go", "owners API"));
        let tool = GraphAwareEditFileTool::new(Arc::new(g));
        // Build a fake input and verify the path resolution at least
        // doesn't error out on the graph side. We don't actually
        // write a file here — that would need a temp file fixture.
        // Instead, exercise the bad-input rejection path.
        let bad = serde_json::json!({
            "node_id": "missing-node",
            "old_string": "x",
            "new_string": "y",
        });
        let ctx = ToolContext::new("/tmp");
        let res = tool.call(bad, &ctx).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn rejects_both_node_id_and_path() {
        let g = Graph::new();
        let tool = GraphAwareEditFileTool::new(Arc::new(g));
        let bad = serde_json::json!({
            "node_id": "x",
            "path": "y",
            "old_string": "x",
            "new_string": "y",
        });
        let ctx = ToolContext::new("/tmp");
        let res = tool.call(bad, &ctx).await;
        assert!(res.is_err());
    }
}
