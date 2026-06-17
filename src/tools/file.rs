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
