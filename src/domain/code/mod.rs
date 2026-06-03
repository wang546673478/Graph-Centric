//! Code domain — concrete implementation of the [`crate::domain`] traits
//! for source-code projects.
//!
//! Phase 1 ships only a minimal `CodeScanner` that walks a directory and
//! extracts file nodes plus naive (regex-style) import edges. Real AST
//! parsing per language will land in Phase 2 alongside model enrichment.

pub mod ast_scanner;

pub use ast_scanner::CodeScanner;

use super::{CheckResult, DomainValidator, TaskNeeds, ToolDef, ToolRegistry, ValidationOutcome};
use crate::error::Result;
use crate::graph::Graph;
use async_trait::async_trait;

/// Default `ToolRegistry` for code tasks. Maps `TaskNeeds` to a tool list
/// per design doc §7.2.
pub struct CodeToolRegistry;

impl ToolRegistry for CodeToolRegistry {
    fn build_tools(&self, needs: &TaskNeeds) -> Vec<ToolDef> {
        let mut tools = vec![tool_descriptor("read_file", "Read a file by path"),
            tool_descriptor("search_code", "grep/regex search across the project")];
        if needs.can_write {
            tools.push(tool_descriptor("edit_file", "Replace a region of a file"));
            tools.push(tool_descriptor("write_file", "Create or overwrite a file"));
        }
        if needs.can_execute {
            tools.push(tool_descriptor("run_command", "Run a shell command"));
            tools.push(tool_descriptor("run_test", "Run the project test suite"));
        }
        tools
    }
}

fn tool_descriptor(name: &str, description: &str) -> ToolDef {
    ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        schema: serde_json::json!({ "type": "object" }),
    }
}

/// Placeholder validator that just confirms the graph isn't structurally
/// broken. Real implementations will run `cargo check`, `cargo test`,
/// clippy, and additional deterministic backstops per principle #5.
pub struct CodeValidator;

#[async_trait]
impl DomainValidator for CodeValidator {
    async fn validate(&self, graph: &Graph) -> Result<ValidationOutcome> {
        let issues = graph.find_inconsistencies();
        let checks = vec![CheckResult {
            name: "graph_structural_consistency".into(),
            passed: issues.is_empty(),
            details: if issues.is_empty() {
                "no inconsistencies".into()
            } else {
                format!("{} issues: {:?}", issues.len(), issues)
            },
        }];
        Ok(ValidationOutcome {
            passed: checks.iter().all(|c| c.passed),
            checks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_yields_basic_tools() {
        let reg = CodeToolRegistry;
        let tools = reg.build_tools(&TaskNeeds::read_only());
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(!names.contains(&"edit_file"));
    }

    #[test]
    fn write_needs_adds_editor_tools() {
        let reg = CodeToolRegistry;
        let tools = reg.build_tools(&TaskNeeds::read_write());
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"write_file"));
        assert!(!names.contains(&"run_command"));
    }

    #[test]
    fn full_needs_adds_execution() {
        let reg = CodeToolRegistry;
        let tools = reg.build_tools(&TaskNeeds::full());
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"run_command"));
        assert!(names.contains(&"run_test"));
    }
}
