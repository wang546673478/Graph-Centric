//! Hook points into the graph loop, configured from disk via `hooks.toml`.
//!
//! Inspired by xAI's `grok-build` plugin/hook architecture. A hook is a
//! small user-defined program (typically a shell command) that runs at a
//! specific point in the agent loop and receives the event payload as
//! JSON on stdin. Use cases: auto-commit on successful patch, send Slack
//! notification on run start, log every model call, gate propose_patch
//! behind a human approval prompt, etc.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// All hook event kinds. Each corresponds to one integration point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    RunStart,
    RunEnd,
    BeforeProposePatch,
    AfterPatchApplied,
    BeforeSubagent,
    AfterSubagent,
    BeforeVerdict,
}

/// JSON payload delivered to a hook subprocess on its stdin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookPayload {
    pub event: HookEvent,
    pub run_id: String,
    pub timestamp_unix_ms: u128,
    /// Event-specific data. For BeforeProposePatch this is the patch spec;
    /// for AfterSubagent this is the subagent result; etc.
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookAction {
    /// Run a shell command. Stdin = JSON HookPayload. Stdout/stderr logged.
    Shell { command: String, args: Vec<String> },
    /// Append the JSON payload as a line to a file.
    LogFile { path: PathBuf },
    /// Reject the operation with `reason` (e.g. gate BeforeProposePatch on
    /// a CI policy check). Hook returns `{"reject": "..."}` on stdout to
    /// trigger rejection; otherwise it's a pass-through.
    Gate { command: String, args: Vec<String> },
}

/// A single configured hook: enabled + ordered + action spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpec {
    pub action: HookAction,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Lower runs first. Same priority = run in declaration order.
    #[serde(default)]
    pub priority: i32,
}

fn default_true() -> bool { true }

/// Top-level hooks configuration parsed from `hooks.toml` (or `.gc_hooks.toml`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub on_run_start: Vec<HookSpec>,
    #[serde(default)]
    pub on_run_end: Vec<HookSpec>,
    #[serde(default)]
    pub on_before_propose_patch: Vec<HookSpec>,
    #[serde(default)]
    pub on_after_patch_applied: Vec<HookSpec>,
    #[serde(default)]
    pub on_before_subagent: Vec<HookSpec>,
    #[serde(default)]
    pub on_after_subagent: Vec<HookSpec>,
    #[serde(default)]
    pub on_before_verdict: Vec<HookSpec>,
}

impl HooksConfig {
    pub fn load_from_path(path: &std::path::Path) -> std::io::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&s).unwrap_or_default())
    }
    pub fn load_or_default(path: Option<&std::path::Path>) -> Self {
        match path.and_then(|p| Self::load_from_path(p).ok()) {
            Some(c) => c,
            None => Self::default(),
        }
    }
    pub fn specs_for(&self, event: HookEvent) -> &[HookSpec] {
        match event {
            HookEvent::RunStart => &self.on_run_start,
            HookEvent::RunEnd => &self.on_run_end,
            HookEvent::BeforeProposePatch => &self.on_before_propose_patch,
            HookEvent::AfterPatchApplied => &self.on_after_patch_applied,
            HookEvent::BeforeSubagent => &self.on_before_subagent,
            HookEvent::AfterSubagent => &self.on_after_subagent,
            HookEvent::BeforeVerdict => &self.on_before_verdict,
        }
    }
}

/// The runtime hook executor. Cheap to clone (Arc inside).
#[derive(Debug, Clone, Default)]
pub struct HookRegistry {
    inner: Arc<tokio::sync::RwLock<HooksConfig>>,
}

impl HookRegistry {
    pub fn new(cfg: HooksConfig) -> Self {
        Self { inner: Arc::new(tokio::sync::RwLock::new(cfg)) }
    }
    pub async fn replace(&self, cfg: HooksConfig) {
        *self.inner.write().await = cfg;
    }
    /// Fire `event` with `data`. Returns `Err(reason)` if any enabled
    /// `Gate` hook returned `{"reject": "..."}` (first rejection wins).
    /// Other hook kinds log their stdout/stderr but never fail the dispatch.
    pub async fn fire(&self, event: HookEvent, data: serde_json::Value, run_id: &str) -> Result<(), String> {
        let cfg = self.inner.read().await.clone();
        let mut specs = cfg.specs_for(event).to_vec();
        specs.sort_by_key(|s| s.priority);
        let payload = HookPayload {
            event,
            run_id: run_id.to_string(),
            timestamp_unix_ms: now_unix_ms(),
            data,
        };
        let payload_json = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(e) => return Err(format!("hook payload serialize: {e}")),
        };
        for spec in specs.iter().filter(|s| s.enabled) {
            match &spec.action {
                HookAction::Shell { command, args } => {
                    run_subprocess(command, args, &payload_json).await;
                }
                HookAction::LogFile { path } => {
                    append_to_file(path, &payload_json);
                }
                HookAction::Gate { command, args } => {
                    let stdout = match run_subprocess_capture(command, args, &payload_json).await {
                        Ok(s) => s,
                        Err(e) => return Err(format!("gate hook error: {e}")),
                    };
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&stdout) {
                        if let Some(reason) = value.get("reject").and_then(|v| v.as_str()) {
                            return Err(reason.to_string());
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn now_unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

async fn run_subprocess(command: &str, args: &[String], stdin_payload: &str) {
    let mut cmd = Command::new(command);
    cmd.args(args).stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    if let Ok(mut child) = cmd.spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(stdin_payload.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
        let _ = child.wait().await;
    }
}

async fn run_subprocess_capture(command: &str, args: &[String], stdin_payload: &str) -> std::io::Result<String> {
    let mut cmd = Command::new(command);
    cmd.args(args).stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(stdin_payload.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }
    let output = child.wait_with_output().await?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn append_to_file(path: &std::path::Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", line);
    }
}

// =============================================================================
// Hook data helpers — small constructors used by graph_loop integration.
// =============================================================================
//
// These keep the loop's hook-fire sites tidy: each one calls one helper that
// produces the JSON payload. Adding a new event is a 5-line change to:
//   1) HookEvent enum above
//   2) HooksConfig field
//   3) specs_for() match arm
//   4) helper below
//   5) graph_loop fire() site

/// Convenience payload for `BeforeProposePatch`.
pub fn before_propose_patch_payload(patches_preview: &str, round: usize) -> serde_json::Value {
    serde_json::json!({
        "patches_preview": patches_preview,
        "round": round,
    })
}

/// Convenience payload for `AfterPatchApplied`.
pub fn after_patch_applied_payload(applied_patches: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "applied_patches": applied_patches,
    })
}

/// Convenience payload for `BeforeSubagent`.
pub fn before_subagent_payload(task_desc: &str, tools: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "task_desc": task_desc,
        "tools": tools,
    })
}

/// Convenience payload for `AfterSubagent`.
pub fn after_subagent_payload(result_ok: bool, summary: &str) -> serde_json::Value {
    serde_json::json!({
        "result_ok": result_ok,
        "summary": summary,
    })
}

/// Convenience payload for `RunEnd`.
pub fn run_end_payload(outcome: &str, rounds: usize) -> serde_json::Value {
    serde_json::json!({
        "outcome": outcome,
        "rounds": rounds,
    })
}

/// Convenience payload for `RunStart`.
pub fn run_start_payload(task: &str) -> serde_json::Value {
    serde_json::json!({
        "task": task,
    })
}

/// Convenience payload for `BeforeVerdict`.
pub fn before_verdict_payload(graph_nodes: usize, graph_edges: usize) -> serde_json::Value {
    serde_json::json!({
        "graph_nodes": graph_nodes,
        "graph_edges": graph_edges,
    })
}

/// Helper to keep some unrelated types referenced (so unused-import lints
/// stay quiet in callers that haven't grown their hook integration yet).
#[allow(dead_code)]
fn _hook_marker(_h: HookEvent, _m: &HashMap<&str, serde_json::Value>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hook_event_serde_round_trip() {
        for ev in [
            HookEvent::RunStart,
            HookEvent::RunEnd,
            HookEvent::BeforeProposePatch,
            HookEvent::AfterPatchApplied,
            HookEvent::BeforeSubagent,
            HookEvent::AfterSubagent,
            HookEvent::BeforeVerdict,
        ] {
            let s = serde_json::to_string(&ev).unwrap();
            let back: HookEvent = serde_json::from_str(&s).unwrap();
            assert_eq!(back, ev);
        }
    }

    #[test]
    fn hooks_config_loads_minimal_toml() {
        // Minimal TOML on disk should round-trip and produce a config with
        // empty hook lists but no parse failure.
        let toml = r#"
            on_run_start = []
            on_before_propose_patch = []
            [some_other_section_that_is_ignored]
            foo = "bar"
        "#;
        let cfg: HooksConfig = toml::from_str(toml).unwrap();
        assert!(cfg.on_run_start.is_empty());
        assert!(cfg.on_before_propose_patch.is_empty());
        assert!(cfg.on_after_patch_applied.is_empty());
    }

    #[test]
    fn specs_for_dispatches_correctly() {
        let mut cfg = HooksConfig::default();
        cfg.on_run_start.push(HookSpec { action: HookAction::LogFile { path: "x.toml".into() }, enabled: true, priority: 0 });
        cfg.on_before_propose_patch.push(HookSpec { action: HookAction::LogFile { path: "y.toml".into() }, enabled: true, priority: 0 });
        assert_eq!(cfg.specs_for(HookEvent::RunStart).len(), 1);
        assert_eq!(cfg.specs_for(HookEvent::BeforeProposePatch).len(), 1);
        assert_eq!(cfg.specs_for(HookEvent::AfterSubagent).len(), 0);
    }

    #[test]
    fn hook_payload_contains_event_run_id_and_data() {
        let p = HookPayload {
            event: HookEvent::BeforeProposePatch,
            run_id: "run-123".into(),
            timestamp_unix_ms: 1700000000000,
            data: json!({"patch_count": 2}),
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: HookPayload = serde_json::from_str(&s).unwrap();
        assert_eq!(back.run_id, "run-123");
        assert_eq!(back.data["patch_count"], 2);
    }

    #[test]
    fn log_file_action_appends_to_file() {
        let tmp = std::env::temp_dir().join("gc-hook-test.log");
        let _ = std::fs::remove_file(&tmp);
        let _ = append_to_file(&tmp, "{\"a\":1}");
        let _ = append_to_file(&tmp, "{\"a\":2}");
        let contents = std::fs::read_to_string(&tmp).unwrap();
        assert!(contents.contains("{\"a\":1}"));
        assert!(contents.contains("{\"a\":2}"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn fire_with_no_hooks_is_no_op_ok() {
        let reg = HookRegistry::default();
        let r = reg.fire(HookEvent::RunStart, json!({}), "run-1").await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn fire_with_log_hook_writes_file() {
        let tmp = std::env::temp_dir().join("gc-hook-registry.log");
        let _ = std::fs::remove_file(&tmp);
        let cfg = HooksConfig {
            on_run_start: vec![HookSpec {
                action: HookAction::LogFile { path: tmp.clone() },
                enabled: true, priority: 0,
            }],
            ..Default::default()
        };
        let reg = HookRegistry::new(cfg);
        let _ = reg.fire(HookEvent::RunStart, json!({"hello": "world"}), "run-1").await;
        let contents = std::fs::read_to_string(&tmp).unwrap();
        assert!(contents.contains("\"hello\":\"world\""));
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn fire_rejects_when_gate_returns_reject() {
        let cfg = HooksConfig {
            on_run_end: vec![HookSpec {
                action: HookAction::Gate {
                    command: "sh".into(),
                    args: vec!["-c".into(), "echo '{\"reject\":\"nope\"}'".into()],
                },
                enabled: true, priority: 0,
            }],
            ..Default::default()
        };
        let reg = HookRegistry::new(cfg);
        let r = reg.fire(HookEvent::RunEnd, json!({}), "run-1").await;
        assert_eq!(r, Err("nope".to_string()));
    }

    #[tokio::test]
    async fn fire_passes_when_gate_returns_empty_object() {
        let cfg = HooksConfig {
            on_run_end: vec![HookSpec {
                action: HookAction::Gate {
                    command: "sh".into(),
                    args: vec!["-c".into(), "echo '{}'".into()],
                },
                enabled: true, priority: 0,
            }],
            ..Default::default()
        };
        let reg = HookRegistry::new(cfg);
        let r = reg.fire(HookEvent::RunEnd, json!({}), "run-1").await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn disabled_hook_does_not_fire() {
        let tmp = std::env::temp_dir().join("gc-hook-disabled.log");
        let _ = std::fs::remove_file(&tmp);
        let cfg = HooksConfig {
            on_run_start: vec![HookSpec {
                action: HookAction::LogFile { path: tmp.clone() },
                enabled: false,  // <-- key
                priority: 0,
            }],
            ..Default::default()
        };
        let reg = HookRegistry::new(cfg);
        let _ = reg.fire(HookEvent::RunStart, json!({}), "run-1").await;
        let exists = tmp.exists();
        assert!(!exists, "disabled hook must not create the file");
    }

    #[test]
    fn priority_ordering_sorts_low_first() {
        // Verify that a higher-priority spec runs after a lower one — we
        // can't easily observe order from inside one test, but we can
        // confirm the sort key is stable and stable sort preserves the
        // declaration order on ties.
        let mut cfg = HooksConfig {
            on_run_start: vec![
                HookSpec { action: HookAction::LogFile { path: "a".into() }, enabled: true, priority: 5 },
                HookSpec { action: HookAction::LogFile { path: "b".into() }, enabled: true, priority: 1 },
                HookSpec { action: HookAction::LogFile { path: "c".into() }, enabled: true, priority: 3 },
            ],
            ..Default::default()
        };
        cfg.on_run_start.sort_by_key(|s| s.priority);
        assert_eq!(cfg.on_run_start[0].priority, 1);
        assert_eq!(cfg.on_run_start[1].priority, 3);
        assert_eq!(cfg.on_run_start[2].priority, 5);
    }
}