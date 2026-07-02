//! PostExecutionValidator — deterministic check between Task and Review.
//!
//! After the Task phase finishes and before the Review phase runs, an
//! optional `PostExecutionValidator` gets to look at the result of sub-agent
//! execution and decide one of three things:
//!
//! - **Passed** — proceed to Review.
//! - **FailedAsGraphIssue** — the failure mode looks structural (sub-agents
//!   wrote code that doesn't compile, tests referencing missing symbols,
//!   etc.). The GraphLoop surfaces
//!   `LoopState::GraphInvalid { source: PostExecutionValidation }` so the
//!   caller can repair the graph and re-run.
//! - **FailedAsTaskIssue** — the failure is task-level (logic bug,
//!   wrong-but-syntactically-valid output). The loop continues to Review,
//!   which will catch it via its model judge.
//!
//! ## Implementations
//!
//! - [`BashCheckValidator`] — runs a configured shell command (e.g.
//!   `cargo check`, `pytest --collect-only`) and pattern-matches its
//!   output to distinguish graph from task issues.
//! - [`AlwaysPasses`] — for tests; always returns `Passed`.
//!
//! Custom validators implement the trait directly; domain-specific
//! validators (terraform plan, SQL schema linters, …) plug in here.

use super::dispatcher::DispatchOutcome;
use super::graph_loop::{GraphError, L0ErrorType};
use crate::error::Result;
use crate::graph::Graph;
use crate::tools::{AllowAll, BashTool, Policy, Tool, ToolContext};
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::debug;

// ---------------------------------------------------------------------------
// Verdict
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum ValidationVerdict {
    Passed,
    FailedAsGraphIssue { errors: Vec<GraphError> },
    FailedAsTaskIssue { details: String },
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait PostExecutionValidator: Send + Sync {
    /// Run after the Task phase completes. The validator sees the final
    /// graph and the sub-agent outcomes; it decides whether to proceed,
    /// bubble back as a graph issue, or surface a task-level failure.
    async fn validate(
        &self,
        graph: &Graph,
        task_outcome: &DispatchOutcome,
        task_description: &str,
    ) -> Result<ValidationVerdict>;
}

// ---------------------------------------------------------------------------
// AlwaysPasses (for tests)
// ---------------------------------------------------------------------------

/// Validator that unconditionally returns `Passed`. Useful in tests and as
/// a no-op placeholder when validation isn't desired but a value is needed.
#[derive(Debug, Clone, Default)]
pub struct AlwaysPasses;

#[async_trait]
impl PostExecutionValidator for AlwaysPasses {
    async fn validate(
        &self,
        _graph: &Graph,
        _task_outcome: &DispatchOutcome,
        _task_description: &str,
    ) -> Result<ValidationVerdict> {
        Ok(ValidationVerdict::Passed)
    }
}

// ---------------------------------------------------------------------------
// BashCheckValidator
// ---------------------------------------------------------------------------

/// Runs a configured shell command and classifies its outcome.
///
/// **Pass** if the command exits 0 within `timeout_ms`.
///
/// **FailedAsGraphIssue** if it exits non-zero AND its combined stdout +
/// stderr contains any string from `graph_error_patterns` (case-insensitive).
/// These patterns are domain-specific signals that mean "the source doesn't
/// match what the graph said":
///
/// - Rust: `"cannot find function"`, `"unresolved import"`, `"no method named"`
/// - TypeScript: `"cannot find module"`, `"property does not exist"`
/// - Python: `"NameError"`, `"ImportError"`, `"AttributeError"`
///
/// **FailedAsTaskIssue** otherwise — non-zero exit but no graph signal.
/// Probably a logic bug or test assertion failure; the Review phase will
/// catch it.
///
/// ## Helpers
///
/// - [`BashCheckValidator::cargo_check_for`] — pre-configured for Rust projects.
pub struct BashCheckValidator {
    pub command: String,
    pub tool_cwd: PathBuf,
    pub policy: Arc<dyn Policy>,
    pub timeout_ms: u64,
    pub max_output_chars: usize,
    pub graph_error_patterns: Vec<String>,
}

impl BashCheckValidator {
    /// Builder for Rust projects: runs `cargo check` and treats
    /// "cannot find function/type/module/method" + "unresolved import" as
    /// graph-rooted failures.
    pub fn cargo_check_for(cwd: impl Into<PathBuf>) -> Self {
        Self {
            command: "cargo check --message-format=short".into(),
            tool_cwd: cwd.into(),
            // `cargo check` writes to target/ — needs AllowAll, NOT ReadOnly
            // (which would block the build entirely). Callers can override
            // by setting `policy` to a stricter check.
            policy: Arc::new(AllowAll),
            timeout_ms: 300_000, // 5 min
            max_output_chars: 32_000,
            graph_error_patterns: vec![
                "cannot find function".into(),
                "cannot find type".into(),
                "cannot find module".into(),
                "cannot find macro".into(),
                "no method named".into(),
                "no associated function".into(),
                "no associated item".into(),
                "unresolved import".into(),
                "unresolved name".into(),
            ],
        }
    }

    /// Builder for Node/TypeScript projects.
    pub fn tsc_check_for(cwd: impl Into<PathBuf>) -> Self {
        Self {
            command: "npx tsc --noEmit".into(),
            tool_cwd: cwd.into(),
            policy: Arc::new(AllowAll),
            timeout_ms: 300_000,
            max_output_chars: 32_000,
            graph_error_patterns: vec![
                "cannot find module".into(),
                "cannot find name".into(),
                "property does not exist".into(),
                "has no exported member".into(),
                "is not assignable to type".into(),
            ],
        }
    }

    /// v2 spec §5.7: Go builder. Runs `go build ./...` and treats
    /// undefined-symbol and unresolved-package errors as
    /// graph-rooted failures (the L1 description said the symbol
    /// exists, the L2 says it doesn't).
    pub fn go_build_for(cwd: impl Into<PathBuf>) -> Self {
        Self {
            command: "go build ./...".into(),
            tool_cwd: cwd.into(),
            policy: Arc::new(AllowAll),
            timeout_ms: 300_000,
            max_output_chars: 32_000,
            graph_error_patterns: vec![
                "undefined:".into(),
                "undefined symbol".into(),
                "cannot find package".into(),
                "import cycle".into(),
                "undeclared name".into(),
            ],
        }
    }

    /// v2 spec §5.7: Python builder. Runs `python -m py_compile` on
    /// the listed files (callers set `with_command` for module
    /// imports). Treats NameError / ImportError / AttributeError
    /// (when the symbol was supposed to exist) as graph-rooted.
    pub fn python_compile_for(cwd: impl Into<PathBuf>) -> Self {
        Self {
            command: "python -m compileall -q .".into(),
            tool_cwd: cwd.into(),
            policy: Arc::new(AllowAll),
            timeout_ms: 300_000,
            max_output_chars: 32_000,
            graph_error_patterns: vec![
                "NameError".into(),
                "ImportError".into(),
                "ModuleNotFoundError".into(),
                "AttributeError".into(),
                "cannot import name".into(),
            ],
        }
    }

    /// v2 spec §5.7: Java builder. Runs `mvn -q compile` (override
    /// with `with_command` for gradle). Treats "cannot find symbol"
    /// and "package … does not exist" as graph-rooted failures.
    pub fn java_compile_for(cwd: impl Into<PathBuf>) -> Self {
        Self {
            command: "mvn -q -DskipTests compile".into(),
            tool_cwd: cwd.into(),
            policy: Arc::new(AllowAll),
            timeout_ms: 300_000,
            max_output_chars: 32_000,
            graph_error_patterns: vec![
                "cannot find symbol".into(),
                "package ".into(),       // "package X does not exist"
                "does not exist".into(),
                "incompatible types".into(),
                "method does not override".into(),
            ],
        }
    }

    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = command.into();
        self
    }

    pub fn with_policy(mut self, policy: Arc<dyn Policy>) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }

    pub fn with_graph_error_patterns(mut self, patterns: Vec<String>) -> Self {
        self.graph_error_patterns = patterns;
        self
    }
}

#[async_trait]
impl PostExecutionValidator for BashCheckValidator {
    async fn validate(
        &self,
        _graph: &Graph,
        _task_outcome: &DispatchOutcome,
        _task_description: &str,
    ) -> Result<ValidationVerdict> {
        let tool = BashTool::new();
        let ctx = ToolContext::new(self.tool_cwd.clone())
            .with_policy(self.policy.clone())
            .with_max_output(self.max_output_chars);
        let input = serde_json::json!({
            "command": self.command,
            "timeout_ms": self.timeout_ms,
        });
        let out = tool.call(input, &ctx).await?;
        debug!(
            command = %self.command,
            exit_code = ?out.exit_code,
            interrupted = out.interrupted,
            "post-execution validator ran"
        );
        if out.exit_code == Some(0) && !out.interrupted {
            return Ok(ValidationVerdict::Passed);
        }

        // Non-zero (or interrupted). Look for graph-error patterns in the
        // combined content (which already includes both stdout and stderr,
        // per BashTool's output assembly).
        let lower = out.content.to_lowercase();
        let hits: Vec<String> = self
            .graph_error_patterns
            .iter()
            .filter(|p| lower.contains(&p.to_lowercase()))
            .cloned()
            .collect();

        if !hits.is_empty() {
            let detail = format!(
                "post-execution check `{}` failed (exit_code={:?}); \
                 matched graph-error patterns: {}",
                self.command,
                out.exit_code,
                hits.join(", ")
            );
            return Ok(ValidationVerdict::FailedAsGraphIssue {
                errors: vec![GraphError::L0Structural {
                    error_type: L0ErrorType::MissingRelation,
                    detail,
                    related_nodes: Vec::new(),
                    discovered_by: Some("post-execution-validator".into()),
                }],
            });
        }

        Ok(ValidationVerdict::FailedAsTaskIssue {
            details: format!(
                "post-execution check `{}` failed (exit_code={:?}); \
                 no graph-error patterns matched — likely a task-level issue",
                self.command, out.exit_code
            ),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::subagent::SubAgentResult;
    use crate::graph::NodeId;

    fn failing_command() -> &'static str {
        "exit 1"
    }

    fn fail_with_stderr(message: &str) -> String {
        if cfg!(target_os = "windows") {
            format!("echo {message} 1>&2 & exit 1")
        } else {
            format!("echo '{message}' 1>&2; exit 1")
        }
    }

    fn empty_outcome() -> DispatchOutcome {
        DispatchOutcome {
            results: vec![SubAgentResult::ok(
                NodeId::from("t1"),
                "done".into(),
                10,
                100,
            )],
            batches: vec![vec![NodeId::from("t1")]],
            total_subagent_ms: 10,
            total_tokens: 100,
            all_succeeded: true,
            graph_errors: Vec::new(),
        }
    }

    #[tokio::test]
    async fn always_passes_returns_passed() {
        let v = AlwaysPasses;
        let r = v
            .validate(&Graph::new(), &empty_outcome(), "any task")
            .await
            .unwrap();
        assert!(matches!(r, ValidationVerdict::Passed));
    }

    #[tokio::test]
    async fn bash_validator_passed_on_exit_zero() {
        let v = BashCheckValidator {
            command: "echo ok".into(),
            tool_cwd: std::env::current_dir().unwrap(),
            policy: Arc::new(AllowAll),
            timeout_ms: 5_000,
            max_output_chars: 1_000,
            graph_error_patterns: Vec::new(),
        };
        let r = v
            .validate(&Graph::new(), &empty_outcome(), "task")
            .await
            .unwrap();
        assert!(matches!(r, ValidationVerdict::Passed));
    }

    #[tokio::test]
    async fn bash_validator_task_issue_on_nonzero_without_pattern_match() {
        // `false` exits 1 with no output; no patterns match → TaskIssue.
        let v = BashCheckValidator {
            command: failing_command().into(),
            tool_cwd: std::env::current_dir().unwrap(),
            policy: Arc::new(AllowAll),
            timeout_ms: 5_000,
            max_output_chars: 1_000,
            graph_error_patterns: vec!["cannot find".into()],
        };
        let r = v
            .validate(&Graph::new(), &empty_outcome(), "task")
            .await
            .unwrap();
        match r {
            ValidationVerdict::FailedAsTaskIssue { details } => {
                assert!(details.contains("exit_code=Some(1)"));
                assert!(details.contains("no graph-error patterns matched"));
            }
            other => panic!("expected FailedAsTaskIssue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bash_validator_graph_issue_on_pattern_match_in_stderr() {
        // Emit a fake compiler-style error containing "cannot find function"
        // to stderr, then exit non-zero.
        let v = BashCheckValidator {
            command: fail_with_stderr("error: cannot find function foo in this scope"),
            tool_cwd: std::env::current_dir().unwrap(),
            policy: Arc::new(AllowAll),
            timeout_ms: 5_000,
            max_output_chars: 4_000,
            graph_error_patterns: vec![
                "cannot find function".into(),
                "unresolved import".into(),
            ],
        };
        let r = v
            .validate(&Graph::new(), &empty_outcome(), "task")
            .await
            .unwrap();
        match r {
            ValidationVerdict::FailedAsGraphIssue { errors } => {
                assert_eq!(errors.len(), 1);
                match &errors[0] {
                    GraphError::L0Structural {
                        error_type,
                        detail,
                        discovered_by,
                        ..
                    } => {
                        assert!(matches!(error_type, L0ErrorType::MissingRelation));
                        assert!(detail.contains("cannot find function"));
                        assert_eq!(discovered_by.as_deref(), Some("post-execution-validator"));
                    }
                    other => panic!("expected L0Structural, got {other:?}"),
                }
            }
            other => panic!("expected FailedAsGraphIssue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bash_validator_case_insensitive_pattern_match() {
        let v = BashCheckValidator {
            command: fail_with_stderr("ERROR: UNRESOLVED IMPORT foo"),
            tool_cwd: std::env::current_dir().unwrap(),
            policy: Arc::new(AllowAll),
            timeout_ms: 5_000,
            max_output_chars: 4_000,
            graph_error_patterns: vec!["unresolved import".into()],
        };
        let r = v
            .validate(&Graph::new(), &empty_outcome(), "task")
            .await
            .unwrap();
        assert!(matches!(r, ValidationVerdict::FailedAsGraphIssue { .. }));
    }

    #[tokio::test]
    async fn cargo_check_for_builder_sets_sensible_defaults() {
        let v = BashCheckValidator::cargo_check_for("/tmp");
        assert!(v.command.contains("cargo check"));
        assert_eq!(v.tool_cwd, PathBuf::from("/tmp"));
        assert!(
            v.graph_error_patterns
                .iter()
                .any(|p| p == "cannot find function")
        );
        assert!(
            v.graph_error_patterns
                .iter()
                .any(|p| p == "unresolved import")
        );
        assert!(v.timeout_ms >= 60_000);
    }

    #[tokio::test]
    async fn tsc_check_for_builder_sets_typescript_patterns() {
        let v = BashCheckValidator::tsc_check_for("/tmp");
        assert!(v.command.contains("tsc"));
        assert!(
            v.graph_error_patterns
                .iter()
                .any(|p| p == "cannot find module")
        );
        assert!(
            v.graph_error_patterns
                .iter()
                .any(|p| p == "has no exported member")
        );
    }

    #[tokio::test]
    async fn builders_chain_with_overrides() {
        let v = BashCheckValidator::cargo_check_for("/tmp")
            .with_command("cargo check --all-features")
            .with_timeout_ms(60_000)
            .with_graph_error_patterns(vec!["custom pattern".into()]);
        assert_eq!(v.command, "cargo check --all-features");
        assert_eq!(v.timeout_ms, 60_000);
        assert_eq!(v.graph_error_patterns, vec!["custom pattern".to_string()]);
    }
}
