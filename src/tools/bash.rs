//! Bash command execution tool.
//!
//! Spawns a child `bash -c "<command>"` via `tokio::process::Command`, captures
//! stdout/stderr, enforces a timeout, isolates the environment from anything
//! that prompts a TTY (no PAGER, no interactive git editor), and returns a
//! [`ToolOutput`] with tail-truncated content plus a structured side-channel.
//!
//! ### Design choices borrowed from Claude Code (for reference; clean impl)
//!
//! - **`kill_on_drop`** — if the parent task aborts, the child gets a SIGKILL
//!   on `Drop`. Avoids leaked subprocesses.
//! - **stdin closed** — agents must produce non-interactive commands; closing
//!   stdin forces failures rather than hangs.
//! - **`GIT_EDITOR=true`** — `git commit` without `-m` would normally launch
//!   `$EDITOR`; pointing it at the no-op `true` binary makes such mistakes
//!   visible (commit refuses) rather than hanging on a phantom editor.
//! - **`PAGER=cat`** — `git log`, `man`, etc. otherwise stall waiting for `q`.
//! - **`CI=1`** — many tools (npm, gh, cargo) suppress prompts when this is set.
//! - **Tail truncation** — keep the last N chars; failure messages live at
//!   the end of command output.
//!
//! ### Deliberately NOT included in Phase 2 v1
//!
//! - background / `run_in_background`
//! - sandbox wrappers (bwrap / sandbox-exec)
//! - progress streaming via `onProgress`
//! - sed-edit special path
//! - shell selection (zsh vs bash) — fixed `bash` for now

use super::{Tool, ToolContext, ToolOutput, truncate_tail};
use crate::error::{HarnessError, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{debug, info};

/// Default cap when the model doesn't specify a timeout.
const DEFAULT_TIMEOUT_MS: u64 = 120_000; // 2 minutes
/// Hard ceiling on what the model can ask for.
const MAX_TIMEOUT_MS: u64 = 600_000; // 10 minutes

#[derive(Debug, Clone)]
pub struct BashTool {
    pub default_timeout: Duration,
    pub max_timeout: Duration,
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

impl BashTool {
    pub fn new() -> Self {
        Self {
            default_timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            max_timeout: Duration::from_millis(MAX_TIMEOUT_MS),
        }
    }

    pub fn with_default_timeout(mut self, d: Duration) -> Self {
        self.default_timeout = d;
        self
    }

    pub fn with_max_timeout(mut self, d: Duration) -> Self {
        self.max_timeout = d;
        self
    }

    /// Heuristic: does this command look read-only? Errs **strict** — only
    /// commands whose first token is in a known-safe list AND whose body
    /// contains no destructive operators return true. Unknown commands and
    /// anything with pipes / redirections / `$()` / backticks are treated
    /// as not read-only.
    ///
    /// This is intentionally crude. Phase 2 will not try to be a shell
    /// parser. If precision matters, the policy layer should constrain the
    /// command shape via allowlists rather than relying on this heuristic.
    pub fn classify_read_only(command: &str) -> bool {
        let cmd = command.trim();
        if cmd.is_empty() {
            return false;
        }
        // Any operator that could escape the safe-prefix interpretation
        // disqualifies the command. We're deliberately conservative.
        const SUSPICIOUS: &[&str] = &["||", "&&", ">", "<", "|", ";", "$(", "`", "&"];
        for s in SUSPICIOUS {
            if cmd.contains(s) {
                return false;
            }
        }
        let first = cmd.split_whitespace().next().unwrap_or("");

        // Multi-word prefixes (e.g. "git log") are matched against the
        // whole command's start so flags after them still classify as read-only.
        const MULTI_WORD_RO: &[&str] = &[
            // git — observation only
            "git status",
            "git log",
            "git diff",
            "git show",
            "git branch",
            "git remote",
            "git config --get",
            "git rev-parse",
            "git ls-files",
            "git ls-tree",
            // cargo — version queries + check + metadata (writes only to target/)
            "cargo check",
            "cargo build --dry-run",
            "cargo --version",
            "cargo -V",
            "cargo metadata",
            "cargo tree",
            "cargo fmt --check",
            "cargo clippy -- -W warnings",
            "cargo doc --no-deps --no-open",
            // rustc — version queries
            "rustc --version",
            "rustc -V",
            "rustc --print",
            // node / npm / yarn / pnpm — version + list
            "node --version",
            "node -v",
            "npm --version",
            "npm -v",
            "npm list",
            "npm ls",
            "npm outdated",
            "yarn --version",
            "yarn -v",
            "pnpm --version",
            "pnpm -v",
            // python — version + read-only inspection
            "python --version",
            "python -V",
            "python3 --version",
            "python3 -V",
            "pip --version",
            "pip -V",
            "pip list",
            "pip show",
            // go / java / ruby
            "go version",
            "go env",
            "go list",
            "java -version",
            "java --version",
            "javac -version",
            "ruby --version",
            "ruby -v",
            // container / k8s — describe-only
            "docker ps",
            "docker images",
            "docker version",
            "docker info",
            "kubectl get",
            "kubectl describe",
            "kubectl version",
            // misc dev
            "make --version",
            "make -n",       // dry-run; prints commands without executing
            "gh --version",
            "gh auth status",
        ];
        for prefix in MULTI_WORD_RO {
            if cmd.starts_with(prefix) {
                return true;
            }
        }

        const SINGLE_RO: &[&str] = &[
            "ls", "cat", "head", "tail", "less", "more", "wc", "stat", "file", "strings", "find",
            "grep", "rg", "ag", "ack", "locate", "which", "whereis", "tree", "du", "df", "ps",
            "top", "htop", "echo", "printf", "true", "false", "pwd", "whoami", "date", "uname",
            "id", "env", "hostname",
        ];
        SINGLE_RO.contains(&first)
    }
}

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    description: Option<String>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command via `bash -c`. Returns combined stdout + stderr \
         and the exit code. stdin is closed and interactive editors/pagers \
         are disabled, so the command must be non-interactive. Use for CLI \
         tools, listing files, building/testing, exploring the system."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": format!(
                        "Optional timeout in milliseconds. Default {}, max {}.",
                        self.default_timeout.as_millis(),
                        self.max_timeout.as_millis(),
                    )
                },
                "description": {
                    "type": "string",
                    "description": "5–10 word description of what the command does. Used in logs and UI."
                }
            },
            "required": ["command"]
        })
    }

    fn is_read_only(&self, input: &serde_json::Value) -> bool {
        input
            .get("command")
            .and_then(|v| v.as_str())
            .map(Self::classify_read_only)
            .unwrap_or(false)
    }

    async fn call(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let parsed: BashInput = serde_json::from_value(input).map_err(|e| {
            HarnessError::domain(format!("bash: invalid input: {e}"))
        })?;

        let timeout = parsed
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.default_timeout)
            .min(self.max_timeout);

        info!(
            command = %parsed.command,
            description = parsed.description.as_deref().unwrap_or(""),
            timeout_ms = timeout.as_millis() as u64,
            "bash tool invocation"
        );

        let start = Instant::now();

        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(&parsed.command);
        cmd.current_dir(&ctx.cwd);
        // Defang interactive helpers — see module doc.
        cmd.env("GIT_EDITOR", "true");
        cmd.env("EDITOR", "true");
        cmd.env("PAGER", "cat");
        cmd.env("CI", "1");
        cmd.env("CLAUDE_HARNESS", "1");
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| HarnessError::domain(format!("bash spawn failed: {e}")))?;

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        let stdout_task = tokio::spawn(async move {
            let mut buf = String::new();
            if let Some(mut p) = stdout_pipe {
                let _ = p.read_to_string(&mut buf).await;
            }
            buf
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = String::new();
            if let Some(mut p) = stderr_pipe {
                let _ = p.read_to_string(&mut buf).await;
            }
            buf
        });

        let wait_result = tokio::time::timeout(timeout, child.wait()).await;
        let interrupted = wait_result.is_err();
        let exit = if interrupted {
            // Kill on timeout; the pipes will then close and the reader tasks complete.
            let _ = child.kill().await;
            None
        } else {
            wait_result
                .ok()
                .and_then(|r| r.ok())
                .and_then(|s| s.code())
        };

        let stdout = stdout_task.await.unwrap_or_default();
        let stderr = stderr_task.await.unwrap_or_default();
        let duration_ms = start.elapsed().as_millis() as u64;
        debug!(
            exit_code = ?exit,
            interrupted,
            stdout_len = stdout.len(),
            stderr_len = stderr.len(),
            duration_ms,
            "bash tool finished"
        );

        // Compose a model-facing block.
        let mut composed = String::new();
        if !stdout.is_empty() {
            composed.push_str(&stdout);
            if !composed.ends_with('\n') {
                composed.push('\n');
            }
        }
        if !stderr.is_empty() {
            composed.push_str("--- stderr ---\n");
            composed.push_str(&stderr);
            if !composed.ends_with('\n') {
                composed.push('\n');
            }
        }
        if interrupted {
            composed.push_str(&format!(
                "--- timeout ---\nCommand killed after {} ms.\n",
                timeout.as_millis()
            ));
        } else if let Some(code) = exit {
            if code != 0 {
                composed.push_str(&format!("--- exit {code} ---\n"));
            }
        }
        if composed.is_empty() {
            composed.push_str("(no output)\n");
        }

        let (content, truncated) = truncate_tail(&composed, ctx.max_output_chars);

        Ok(ToolOutput {
            content,
            structured: Some(serde_json::json!({
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": exit,
                "truncated": truncated,
                "interrupted": interrupted,
                "duration_ms": duration_ms,
            })),
            truncated,
            exit_code: exit,
            interrupted,
            duration_ms,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_here() -> ToolContext {
        ToolContext::new(std::env::current_dir().unwrap())
    }

    #[tokio::test]
    async fn echo_returns_stdout_and_exit_zero() {
        let t = BashTool::new();
        let out = t
            .call(serde_json::json!({"command": "echo hello"}), &ctx_here())
            .await
            .unwrap();
        assert!(out.content.contains("hello"), "got {:?}", out.content);
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.interrupted);
        assert!(!out.truncated);
    }

    #[tokio::test]
    async fn nonzero_exit_is_reported_with_code() {
        let t = BashTool::new();
        let out = t
            .call(serde_json::json!({"command": "exit 7"}), &ctx_here())
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(7));
        assert!(out.content.contains("exit 7"));
    }

    #[tokio::test]
    async fn stderr_is_captured_under_section_marker() {
        let t = BashTool::new();
        let out = t
            .call(
                serde_json::json!({"command": "echo to-err 1>&2"}),
                &ctx_here(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("to-err"));
        assert!(out.content.contains("--- stderr ---"));
    }

    #[tokio::test]
    async fn timeout_kills_process_and_marks_interrupted() {
        let t = BashTool::new();
        let out = t
            .call(
                serde_json::json!({"command": "sleep 30", "timeout_ms": 200}),
                &ctx_here(),
            )
            .await
            .unwrap();
        assert!(out.interrupted);
        assert!(out.content.contains("timeout"));
    }

    #[tokio::test]
    async fn cwd_is_respected() {
        let t = BashTool::new();
        let tmp = std::env::temp_dir();
        let ctx = ToolContext::new(tmp.clone());
        let out = t
            .call(serde_json::json!({"command": "pwd"}), &ctx)
            .await
            .unwrap();
        // /tmp on macOS is symlinked to /private/tmp; accept either prefix.
        let pwd_out = out.content.trim();
        let tmp_str = tmp.to_string_lossy().to_string();
        assert!(
            pwd_out.contains(&tmp_str)
                || pwd_out.contains(tmp_str.trim_start_matches("/private")),
            "pwd output {pwd_out:?} doesn't match cwd {tmp_str:?}"
        );
    }

    #[tokio::test]
    async fn structured_side_channel_present() {
        let t = BashTool::new();
        let out = t
            .call(serde_json::json!({"command": "echo x"}), &ctx_here())
            .await
            .unwrap();
        let s = out.structured.unwrap();
        assert_eq!(s.get("exit_code").and_then(|v| v.as_i64()), Some(0));
        assert!(s.get("stdout").and_then(|v| v.as_str()).unwrap().contains("x"));
    }

    #[tokio::test]
    async fn invalid_input_errors_with_message() {
        let t = BashTool::new();
        let err = t
            .call(serde_json::json!({"no_command": true}), &ctx_here())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("invalid input"));
    }

    #[tokio::test]
    async fn output_is_tail_truncated_when_exceeding_cap() {
        let t = BashTool::new();
        let ctx = ToolContext::new(std::env::current_dir().unwrap()).with_max_output(200);
        // Generate a long output: 'seq 1 500' prints "1\n2\n…500\n"
        let out = t
            .call(serde_json::json!({"command": "seq 1 500"}), &ctx)
            .await
            .unwrap();
        assert!(out.truncated);
        assert!(out.content.starts_with("[…"));
        // Tail should contain the highest numbers
        assert!(out.content.contains("500"));
    }

    #[test]
    fn classify_read_only_basics() {
        assert!(BashTool::classify_read_only("ls"));
        assert!(BashTool::classify_read_only("ls -la /tmp"));
        assert!(BashTool::classify_read_only("cat file.txt"));
        assert!(BashTool::classify_read_only("git status"));
        assert!(BashTool::classify_read_only("git log -10"));
        assert!(BashTool::classify_read_only("pwd"));

        assert!(!BashTool::classify_read_only(""));
        assert!(!BashTool::classify_read_only("rm -rf /"));
        assert!(!BashTool::classify_read_only("echo hi > file")); // redirect
        assert!(!BashTool::classify_read_only("ls | head")); // pipe
        assert!(!BashTool::classify_read_only("ls && rm")); // compound
        assert!(!BashTool::classify_read_only("echo $(date)")); // subshell
        assert!(!BashTool::classify_read_only("foo")); // unknown command
    }

    #[test]
    fn classify_read_only_dev_tool_versions() {
        // Version queries are universally read-only.
        assert!(BashTool::classify_read_only("rustc --version"));
        assert!(BashTool::classify_read_only("rustc -V"));
        assert!(BashTool::classify_read_only("cargo --version"));
        assert!(BashTool::classify_read_only("node --version"));
        assert!(BashTool::classify_read_only("npm --version"));
        assert!(BashTool::classify_read_only("python --version"));
        assert!(BashTool::classify_read_only("python3 -V"));
        assert!(BashTool::classify_read_only("go version"));
        assert!(BashTool::classify_read_only("java -version"));
        assert!(BashTool::classify_read_only("ruby --version"));
        assert!(BashTool::classify_read_only("docker version"));
        assert!(BashTool::classify_read_only("kubectl version"));
    }

    #[test]
    fn classify_read_only_cargo_check_and_metadata() {
        assert!(BashTool::classify_read_only("cargo check"));
        assert!(BashTool::classify_read_only("cargo check --all-features"));
        assert!(BashTool::classify_read_only("cargo metadata"));
        assert!(BashTool::classify_read_only("cargo tree"));
        assert!(BashTool::classify_read_only("cargo fmt --check"));
        assert!(BashTool::classify_read_only("cargo doc --no-deps --no-open"));
    }

    #[test]
    fn classify_read_only_inspection_commands() {
        assert!(BashTool::classify_read_only("npm list"));
        assert!(BashTool::classify_read_only("npm outdated"));
        assert!(BashTool::classify_read_only("pip list"));
        assert!(BashTool::classify_read_only("pip show requests"));
        assert!(BashTool::classify_read_only("go env"));
        assert!(BashTool::classify_read_only("go list ./..."));
        assert!(BashTool::classify_read_only("docker ps"));
        assert!(BashTool::classify_read_only("docker images"));
        assert!(BashTool::classify_read_only("kubectl get pods"));
        assert!(BashTool::classify_read_only("kubectl describe pod foo"));
        assert!(BashTool::classify_read_only("gh auth status"));
    }

    #[test]
    fn classify_read_only_rejects_dev_tool_mutations() {
        // The whitelist must NOT let through obviously-mutating commands.
        assert!(!BashTool::classify_read_only("cargo build"));
        assert!(!BashTool::classify_read_only("cargo run"));
        assert!(!BashTool::classify_read_only("cargo test"));
        assert!(!BashTool::classify_read_only("cargo install foo"));
        assert!(!BashTool::classify_read_only("rustc foo.rs -o bar"));
        assert!(!BashTool::classify_read_only("npm install lodash"));
        assert!(!BashTool::classify_read_only("npm publish"));
        assert!(!BashTool::classify_read_only("pip install requests"));
        assert!(!BashTool::classify_read_only("docker run alpine"));
        assert!(!BashTool::classify_read_only("kubectl apply -f manifest.yaml"));
        assert!(!BashTool::classify_read_only("python script.py"));
    }

    #[test]
    fn schema_has_required_command_field() {
        let t = BashTool::new();
        let s = t.input_schema();
        assert_eq!(
            s.get("required").and_then(|v| v.as_array()),
            Some(&vec![serde_json::Value::String("command".into())])
        );
        assert!(s.get("properties").and_then(|p| p.get("command")).is_some());
    }
}
