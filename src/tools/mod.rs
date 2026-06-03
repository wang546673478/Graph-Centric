//! Tool execution layer.
//!
//! Design distilled from Claude Code's tool architecture (their public npm
//! package, reconstructed from sourcemap; used here as a *reference* for
//! engineering patterns — no code is copied verbatim).
//!
//! ## Concepts
//!
//! - [`Tool`] — the execution trait. Each implementation owns: a name, a
//!   model-facing description, an input JSON schema, classification helpers
//!   (`is_read_only` / `is_destructive` / `is_concurrency_safe`), and the
//!   async `call` method.
//! - [`ToolDef`] — the *model-facing* serialization of a tool (name +
//!   description + JSON schema). Carried over the wire to the LLM in the
//!   `tools` array of a chat request.
//! - [`ToolContext`] — runtime context passed to every call: cwd, policy,
//!   output cap.
//! - [`Policy`] — decides whether a call may proceed. `AllowAll` and
//!   `ReadOnly` are stock implementations; downstream wiring (Phase 3) can
//!   produce policies that ask the user for confirmation.
//! - [`ToolRegistry`] — name → `Arc<dyn Tool>` lookup. All invocations go
//!   through the registry so the policy gate is the single entry point.
//! - [`truncate_tail`] — when output exceeds the cap, keep the **tail** with
//!   a marker. Errors and the last lines of long logs almost always live at
//!   the end; head-truncation would be the wrong default.
//!
//! ## What we intentionally don't ship in Phase 2 v1
//!
//! - background tasks, progress streaming, sandbox wrappers, MCP tools,
//!   per-tool persistence-to-disk for large outputs. These will land
//!   alongside Phase 3 / Phase 4 as the orchestrator needs them.

pub mod bash;
pub mod deny_list;
pub mod scope_guard;

pub use bash::BashTool;
pub use deny_list::{default_dangerous_patterns, match_denial, DenialMatcher, DenialPattern};
pub use scope_guard::{ScopeGuard, ScopeViolation};

use crate::error::{HarnessError, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Model-facing tool definition
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Execution-side result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Human-readable / model-facing text. May be a tail-truncated form of
    /// the raw output.
    pub content: String,
    /// Structured side-channel data (e.g. `{stdout, stderr, exit_code}`)
    /// the runtime may use without parsing `content`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured: Option<serde_json::Value>,
    /// True if `content` was tail-truncated to fit the cap.
    pub truncated: bool,
    /// Process exit code if applicable. `None` for tools that don't run a
    /// process, or when the process was killed before exiting.
    pub exit_code: Option<i32>,
    /// True if the tool was forcibly stopped (timeout / cancel).
    pub interrupted: bool,
    pub duration_ms: u64,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>, exit_code: Option<i32>) -> Self {
        Self {
            content: content.into(),
            structured: None,
            truncated: false,
            exit_code,
            interrupted: false,
            duration_ms: 0,
        }
    }

    pub fn interrupted(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            structured: None,
            truncated: false,
            exit_code: None,
            interrupted: true,
            duration_ms: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Policy gate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum PolicyDecision {
    Allow,
    Deny(String),
    /// Tool call is gated on user confirmation. Phase 2 v1 treats this as
    /// "deny with reason"; Phase 3 will surface the question to the user
    /// through the conversation loop.
    AskUser(String),
}

pub trait Policy: Send + Sync {
    fn decide(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        is_read_only: bool,
    ) -> PolicyDecision;
}

/// Permit every call. Useful for tests and trusted local runs.
#[derive(Debug, Clone, Default)]
pub struct AllowAll;

impl Policy for AllowAll {
    fn decide(&self, _: &str, _: &serde_json::Value, _: bool) -> PolicyDecision {
        PolicyDecision::Allow
    }
}

/// Permit only calls the tool itself classifies as read-only. Useful as a
/// conservative default when running against untrusted tasks.
#[derive(Debug, Clone, Default)]
pub struct ReadOnly;

impl Policy for ReadOnly {
    fn decide(
        &self,
        _tool: &str,
        _input: &serde_json::Value,
        is_read_only: bool,
    ) -> PolicyDecision {
        if is_read_only {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Deny("read-only policy".into())
        }
    }
}

/// Permit only a fixed allowlist of tool names, regardless of input.
pub struct AllowList(pub Vec<String>);

impl Policy for AllowList {
    fn decide(
        &self,
        tool_name: &str,
        _input: &serde_json::Value,
        _is_read_only: bool,
    ) -> PolicyDecision {
        if self.0.iter().any(|n| n == tool_name) {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Deny(format!("{tool_name} not in allowlist"))
        }
    }
}

// ---------------------------------------------------------------------------
// DangerousCommandDeny — a precise deny-list policy for the bash tool.
// ---------------------------------------------------------------------------

/// A `Policy` that allows every call by default, but denies any call to
/// the `bash` tool whose `command` matches a [`DenialPattern`]. Other
/// tools are unaffected.
///
/// v1 ships with a built-in library of high-risk patterns
/// ([`default_dangerous_patterns`]); use `with_pattern` / `without_pattern`
/// to tune.
///
/// Note: this Policy makes a coarse per-call decision. The complementary
/// [`ScopeGuard`](crate::tools::ScopeGuard) handles write-scope checking.
pub struct DangerousCommandDeny {
    patterns: Vec<DenialPattern>,
}

impl std::fmt::Debug for DangerousCommandDeny {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DangerousCommandDeny")
            .field("patterns", &self.patterns.iter().map(|p| &p.name).collect::<Vec<_>>())
            .finish()
    }
}

impl Default for DangerousCommandDeny {
    fn default() -> Self {
        Self::new()
    }
}

impl DangerousCommandDeny {
    /// Create a policy that allows everything except matches against the
    /// built-in [`default_dangerous_patterns`] library.
    pub fn new() -> Self {
        Self {
            patterns: default_dangerous_patterns(),
        }
    }

    /// Append a single custom pattern.
    pub fn with_pattern(mut self, name: impl Into<String>, m: DenialMatcher) -> Self {
        self.patterns.push(DenialPattern { name: name.into(), matcher: m });
        self
    }

    /// Append several custom patterns at once.
    pub fn with_patterns(mut self, extra: Vec<DenialPattern>) -> Self {
        self.patterns.extend(extra);
        self
    }

    /// Remove a built-in pattern by name. Use when a default is too
    /// aggressive for a particular environment. (For the common case of
    /// wanting to *narrow* the deny-list, prefer `with_pattern` over this.)
    pub fn without_pattern(mut self, name: &str) -> Self {
        self.patterns.retain(|p| p.name != name);
        self
    }

    /// Names of every active pattern — for tests, debugging, and audit.
    pub fn pattern_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.patterns.iter()
            .map(|p| p.name.clone()).collect();
        names.sort();
        names
    }
}

impl Policy for DangerousCommandDeny {
    fn decide(
        &self,
        tool: &str,
        input: &serde_json::Value,
        _is_read_only: bool,
    ) -> PolicyDecision {
        if tool != "bash" {
            return PolicyDecision::Allow;
        }
        let cmd = input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        for p in &self.patterns {
            if match_denial(&p.matcher, cmd) {
                return PolicyDecision::Deny(format!(
                    "blocked by dangerous-command pattern '{}'", p.name
                ));
            }
        }
        PolicyDecision::Allow
    }
}

// ---------------------------------------------------------------------------
// Runtime context
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub policy: Arc<dyn Policy>,
    pub max_output_chars: usize,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("cwd", &self.cwd)
            .field("max_output_chars", &self.max_output_chars)
            .finish()
    }
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            policy: Arc::new(AllowAll),
            max_output_chars: 30_000,
        }
    }

    pub fn with_policy(mut self, policy: Arc<dyn Policy>) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_max_output(mut self, n: usize) -> Self {
        self.max_output_chars = n;
        self
    }
}

// ---------------------------------------------------------------------------
// Tool trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    /// Short description shown to the model. ~1 sentence.
    fn description(&self) -> &str;

    /// JSON schema describing the call's input. Should be a JSON Schema
    /// `type: object` with `properties` and `required` fields.
    fn input_schema(&self) -> serde_json::Value;

    /// Per-call classification: does this invocation only observe state?
    /// Default false — only well-known-safe tools should override.
    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }

    /// Per-call classification: does this invocation perform irreversible
    /// operations (delete, overwrite, send)? Defaults to `!is_read_only`.
    fn is_destructive(&self, input: &serde_json::Value) -> bool {
        !self.is_read_only(input)
    }

    /// Per-call classification: can this run concurrently with other tools?
    /// Defaults to `is_read_only` — safe-by-default conservatism.
    fn is_concurrency_safe(&self, input: &serde_json::Value) -> bool {
        self.is_read_only(input)
    }

    async fn call(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;

    /// Convert this tool's metadata into a model-facing definition.
    fn to_def(&self) -> ToolDef {
        ToolDef {
            name: self.name().to_string(),
            description: self.description().to_string(),
            schema: self.input_schema(),
        }
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools.values().map(|t| t.to_def()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        names.sort();
        names
    }

    /// Invoke a tool by name. Routes through `ctx.policy` first; only
    /// `PolicyDecision::Allow` proceeds to the tool's `call`.
    pub async fn invoke(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let tool = self
            .get(name)
            .ok_or_else(|| HarnessError::domain(format!("unknown tool: {name}")))?;
        let is_ro = tool.is_read_only(&input);
        match ctx.policy.decide(name, &input, is_ro) {
            PolicyDecision::Allow => tool.call(input, ctx).await,
            PolicyDecision::Deny(reason) => Err(HarnessError::domain(format!(
                "policy denied {name}: {reason}"
            ))),
            PolicyDecision::AskUser(reason) => Err(HarnessError::domain(format!(
                "policy needs user confirmation for {name}: {reason}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Tail truncation
// ---------------------------------------------------------------------------

/// Truncate `text` so it occupies at most `max_chars` characters, preserving
/// the **tail**. When truncation happens, a one-line marker is prepended
/// reporting how many chars were dropped from the head.
///
/// Why keep the tail: command/test/log output convention is that the
/// interesting bits (errors, summaries, exit messages) sit at the end.
/// Head-truncation would routinely hide the reason for failure.
pub fn truncate_tail(text: &str, max_chars: usize) -> (String, bool) {
    let total = text.chars().count();
    if total <= max_chars {
        return (text.to_string(), false);
    }
    if max_chars == 0 {
        return (format!("[…{total} chars truncated…]\n"), true);
    }
    let skip = total - max_chars;
    let tail: String = text.chars().skip(skip).collect();
    (format!("[…{skip} chars truncated…]\n{tail}"), true)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_unchanged() {
        let (s, t) = truncate_tail("hello", 100);
        assert_eq!(s, "hello");
        assert!(!t);
    }

    #[test]
    fn truncate_keeps_tail_with_marker() {
        let big: String = "ab".repeat(500);
        let (out, truncated) = truncate_tail(&big, 100);
        assert!(truncated);
        assert!(out.starts_with("[…"));
        assert!(out.ends_with("ab"));
        // The tail content should be the last 100 chars
        let body = out.split_once('\n').map(|(_, b)| b).unwrap_or("");
        assert_eq!(body.chars().count(), 100);
    }

    #[test]
    fn truncate_handles_unicode_boundary() {
        let s = "图认知引擎".repeat(50); // 5 chars × 50 = 250 chars
        let (out, truncated) = truncate_tail(&s, 30);
        assert!(truncated);
        let body = out.split_once('\n').map(|(_, b)| b).unwrap_or("");
        // Tail-truncation by chars (not bytes) must not panic on multi-byte boundaries
        assert_eq!(body.chars().count(), 30);
    }

    #[test]
    fn readonly_policy_denies_writes() {
        let p = ReadOnly;
        match p.decide("bash", &serde_json::json!({}), false) {
            PolicyDecision::Deny(_) => {}
            _ => panic!("expected Deny"),
        }
        match p.decide("bash", &serde_json::json!({}), true) {
            PolicyDecision::Allow => {}
            _ => panic!("expected Allow"),
        }
    }

    #[test]
    fn allowlist_filters_by_name() {
        let p = AllowList(vec!["bash".into()]);
        assert_eq!(p.decide("bash", &serde_json::json!({}), false), PolicyDecision::Allow);
        match p.decide("rm_files", &serde_json::json!({}), false) {
            PolicyDecision::Deny(_) => {}
            _ => panic!("expected Deny"),
        }
    }

    #[test]
    fn registry_defs_round_trip() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(BashTool::new()));
        let defs = reg.defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "bash");
        assert!(defs[0].schema.get("properties").is_some());
        assert_eq!(reg.names(), vec!["bash".to_string()]);
    }

    #[tokio::test]
    async fn registry_invoke_routes_through_policy() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(BashTool::new()));
        let ctx = ToolContext::new(std::env::current_dir().unwrap())
            .with_policy(Arc::new(ReadOnly));
        // `rm -rf /tmp/nope` is not read-only — should be denied without spawning.
        let err = reg
            .invoke(
                "bash",
                serde_json::json!({"command": "rm -rf /tmp/xyz-nonexistent"}),
                &ctx,
            )
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("policy denied"), "unexpected: {msg}");
    }

    #[tokio::test]
    async fn registry_invoke_unknown_tool_errors() {
        let reg = ToolRegistry::new();
        let ctx = ToolContext::new(std::env::current_dir().unwrap());
        let err = reg.invoke("nope", serde_json::json!({}), &ctx).await.unwrap_err();
        assert!(format!("{err}").contains("unknown tool"));
    }
}
