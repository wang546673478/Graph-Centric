//! Write-scope guard: constrain a sub-agent's write operations to a set
//! of allowed paths derived from the task's `involved_nodes`.
//!
//! The complementary guard to [`DangerousCommandDeny`](crate::tools::DangerousCommandDeny).
//! That policy decides whether a *command* is dangerous; this one decides
//! whether the *target of a write* is in the task's allowed scope.
//!
//! ## Build tool support
//!
//! Common build tools (`cargo`, `npm`, `pip`, `make`, `go`, `python`,
//! `node`, `rustc`, etc.) are recognized as "implicit cwd writes":
//! they are treated as write intent for scope checking, but when no
//! explicit path is given in the command, the scope check is skipped
//! (we assume the tool writes to a cwd-based subdirectory like
//! `target/`, `node_modules/`, `dist/`, etc., which is in scope when
//! the agent's cwd is).
//!
//! The default verb list is `default_implicit_cwd_verbs()` and is
//! runtime-configurable via `with_implicit_cwd_verb` /
//! `without_implicit_cwd_verb` / `reset_implicit_cwd_verbs`.
//! Project-specific tools (cmake, gradle, mvn, bundle, gem, etc.)
//! can be added; tools known to write outside cwd (e.g., `pip` in
//! projects using virtualenvs) can be removed.
//!
//! **Known v1.1 limitations** (also disclosed in README): system-install
//! commands like `cargo install`, `pip install`, `npm install -g` are
//! allowed by this rule (they write outside cwd but we can't tell).
//! Stricter environments can remove the corresponding verb from
//! `implicit_cwd_verbs` via `without_implicit_cwd_verb`.

use crate::graph::{Graph, Node, NodeId};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// A write-scope check, derived from a task's `involved_nodes` or built
/// from an explicit path list.
///
/// Read operations (and the bash tool's read-only commands) pass
/// unconditionally. Write operations must target a path that lies within
/// at least one of [`allowed_write_paths`](Self::allowed_write_paths).
///
/// Construct via:
/// - [`ScopeGuard::new`] for a fixed list of allowed paths (tests, scripting)
/// - [`ScopeGuard::from_involved_nodes`] for the common case
#[derive(Debug, Clone)]
pub struct ScopeGuard {
    /// Allow a write if the resolved target path starts with one of these.
    pub allowed_write_paths: Vec<PathBuf>,
    /// If true, read operations are also restricted to the allowed set.
    /// Default false: reads are unconstrained because "exploring the world"
    /// is a legitimate model behavior.
    pub restrict_reads: bool,
    /// Verbs (first whitespace-separated token) recognized as
    /// "implicit cwd writes" — the scope check is skipped when
    /// no explicit path is in the command. Configurable via
    /// `with_implicit_cwd_verb` / `without_implicit_cwd_verb` /
    /// `reset_implicit_cwd_verbs`. Initialized from
    /// `default_implicit_cwd_verbs()`.
    pub implicit_cwd_verbs: Vec<String>,
}

/// A failed scope check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeViolation {
    pub tool: String,
    pub reason: String,
    pub offending_paths: Vec<PathBuf>,
}

impl fmt::Display for ScopeViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "scope violation in tool `{}`: {}", self.tool, self.reason)
    }
}

impl std::error::Error for ScopeViolation {}

impl ScopeGuard {
    /// Build a guard from an explicit allow-list of write paths. Each
    /// path is treated as a directory prefix: writes anywhere underneath
    /// it are allowed. Pass absolute paths.
    pub fn new(allowed_write_paths: Vec<PathBuf>) -> Self {
        Self {
            allowed_write_paths,
            restrict_reads: false,
            implicit_cwd_verbs: Self::default_implicit_cwd_verbs(),
        }
    }

    /// Set whether reads are also restricted to the allowed set.
    /// Default false.
    pub fn restrict_reads(mut self, yes: bool) -> Self {
        self.restrict_reads = yes;
        self
    }

    /// Add a verb to the implicit-cwd-write list. The verb is matched
    /// case-sensitively against the first whitespace-separated token
    /// of the command. Idempotent (no duplicates).
    pub fn with_implicit_cwd_verb(mut self, verb: impl Into<String>) -> Self {
        let v = verb.into();
        if !self.implicit_cwd_verbs.contains(&v) {
            self.implicit_cwd_verbs.push(v);
        }
        self
    }

    /// Remove a verb from the implicit-cwd-write list. Useful for
    /// stripping tools that are known to write outside cwd (e.g.,
    /// "pip" for a project that uses virtualenvs).
    pub fn without_implicit_cwd_verb(mut self, verb: &str) -> Self {
        self.implicit_cwd_verbs.retain(|v| v != verb);
        self
    }

    /// Reset the implicit-cwd-write list to the default. Useful when
    /// the caller has applied a long chain of `with_/without_` and
    /// wants to start fresh.
    pub fn reset_implicit_cwd_verbs(mut self) -> Self {
        self.implicit_cwd_verbs = Self::default_implicit_cwd_verbs();
        self
    }

    /// Derive allowed write paths from a set of world-graph nodes. Each
    /// node's `path` is taken; non-file-shaped nodes contribute nothing.
    pub fn from_involved_nodes(
        graph: &Graph,
        involved: &[NodeId],
    ) -> Self {
        let mut paths: Vec<PathBuf> = Vec::new();
        for id in involved {
            if let Some(node) = graph.get_node(id) {
                Self::collect_paths_from_node(node, &mut paths);
            }
        }
        // Also walk distance-1 neighbors in case the L0 references
        // structures that span modules.
        // `Graph::neighbors` returns `Iterator<Item = &NodeId>`; we
        // clone to own the ids.
        let neighbors: Vec<NodeId> = involved
            .iter()
            .flat_map(|id| graph.neighbors(id).cloned())
            .collect();
        for id in &neighbors {
            if let Some(node) = graph.get_node(id) {
                Self::collect_paths_from_node(node, &mut paths);
            }
        }
        // Deduplicate (paths may repeat when multiple nodes share a parent).
        paths.sort();
        paths.dedup();
        // If no file paths were found (all Task-kind nodes, e.g. self-optimization),
        // allow the project root so the agent can work on the full codebase.
        if paths.is_empty() {
            return Self::new(vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))])
                .restrict_reads(false);
        }
        Self::new(paths)
    }

    fn collect_paths_from_node(node: &Node, out: &mut Vec<PathBuf>) {
        // `Node::path` always exists, even for non-file kinds (it's the
        // node's id-as-path by default). For Task/Other nodes we skip.
        match node.kind {
            crate::graph::NodeKind::Task => return,
            _ => {}
        }
        let p = PathBuf::from(&node.path);
        if p.as_os_str().is_empty() {
            return;
        }
        out.push(p);
    }

    /// Check whether the given (tool, input) is in scope. Returns Ok(())
    /// for permitted calls, `Err(ScopeViolation)` for denied.
    pub fn check(
        &self,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<(), ScopeViolation> {
        // For non-bash tools v1 is permissive — the safety story is
        // "default-deny via DangerousCommandDeny" + "scope is about
        // bash writes specifically." A future EditFile/WriteFile tool
        // would call into a sibling `check_path` helper.
        if tool != "bash" {
            return Ok(());
        }
        let cmd = input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        self.check_bash(cmd, tool)
    }

    /// True if `path` lies within one of the allowed prefixes. The
    /// comparison uses `starts_with`. No canonicalization is performed
    /// — call sites that care about symlinks / `..` resolution must pass
    /// already-canonicalized paths.
    pub fn path_is_allowed(&self, path: &Path) -> bool {
        let path = path.to_path_buf();
        for prefix in &self.allowed_write_paths {
            if path.starts_with(prefix) {
                return true;
            }
        }
        false
    }

    /// True if this guard constrains anything (used by callers to skip
    /// the check entirely when no scope was set).
    pub fn is_active(&self) -> bool {
        !self.allowed_write_paths.is_empty()
    }

    /// Default list of build tools that write to cwd-based implicit
    /// locations. Used to initialize [`ScopeGuard::implicit_cwd_verbs`].
    ///
    /// The set is intentionally conservative — it covers the most common
    /// languages' build/test commands. Project-specific tools (cmake,
    /// gradle, mvn, bundle, gem, meson, etc.) can be added at construction
    /// time via `with_implicit_cwd_verb` or by telling the model about
    /// the configuration surface.
    pub fn default_implicit_cwd_verbs() -> Vec<String> {
        vec![
            // Rust
            "cargo".into(), "rustc".into(),
            // Go
            "go".into(),
            // JavaScript / Node
            "node".into(), "npm".into(), "yarn".into(), "pnpm".into(),
            // Python
            "python".into(), "python3".into(), "pip".into(), "pip3".into(),
            // Generic build
            "make".into(),
        ]
    }

    /// The internal bash-specific check. Public to allow focused tests.
    ///
    /// Algorithm (10 steps):
    /// 1. classify read vs write
    /// 2. (read-only path) extract paths and scope-check them, honoring `restrict_reads`
    /// 3. detect write intent (file-level mutation verb or redirect)
    /// 4. detect implicit-cwd write intent (build tool verb)
    /// 5. compound-operator check (runs BEFORE the "unrecognized" check)
    /// 6. combined "unrecognized" check (neither write_intent nor implicit_cwd)
    /// 7. extract file paths from the command
    /// 8. NEW: implicit-cwd write with no explicit path is allowed
    /// 9. explicit write with no path is still an error
    /// 10. scope check each path against the allowed set
    pub fn check_bash(&self, cmd: &str, tool: &str) -> Result<(), ScopeViolation> {
        // Step 1: classify read vs write.
        let is_ro = crate::tools::BashTool::classify_read_only(cmd);

        // Step 2: read-only path — honor `restrict_reads` and the read's
        // compound-operator tolerance. We deliberately skip the
        // compound-operator check here — a read is a read regardless of
        // how it's composed (e.g. `cat /etc/passwd | grep foo` is a
        // legitimate compound read, not a write in disguise). This logic
        // is preserved from the pre-rework implementation.
        if is_ro {
            if !self.restrict_reads {
                // Reads are unconstrained when restrict_reads is off.
                return Ok(());
            }
            // Reads are also restricted: extract paths and check each
            // against the allowed set.
            let paths = Self::extract_paths(cmd);
            let real_paths: Vec<PathBuf> = paths
                .into_iter()
                .filter(|p| !Self::is_path_traversal(p))
                .collect();
            let mut offending: Vec<PathBuf> = Vec::new();
            for p in &real_paths {
                if !self.path_is_allowed(p) {
                    offending.push(p.clone());
                }
            }
            if !offending.is_empty() {
                let first = offending[0].display().to_string();
                return Err(ScopeViolation {
                    tool: tool.into(),
                    reason: format!("read path outside allowed scope: {}", first),
                    offending_paths: offending,
                });
            }
            // No offending paths (or no paths at all — e.g. `ls`, `pwd`).
            return Ok(());
        }

        // Step 3: detect write intent (file-level mutation verb or redirect).
        let write_intent = Self::detect_write_intent(cmd);

        // Step 4: detect implicit-cwd write intent (build tool verb).
        let implicit_cwd = self.is_implicit_cwd_write_verb(cmd);

        // Step 5: compound operator check (moved UP from the pre-rework
        // step 4; runs before the "unrecognized" check per user direction).
        if Self::has_compound_operator(cmd) {
            return Err(ScopeViolation {
                tool: tool.into(),
                reason: "command shape too complex to scope-check; split into separate calls".into(),
                offending_paths: Vec::new(),
            });
        }

        // Step 6: combined "unrecognized" check — both write_intent and
        // implicit_cwd must be false for the command shape to be
        // unrecognizable.
        if !write_intent && !implicit_cwd {
            return Err(ScopeViolation {
                tool: tool.into(),
                reason: "command shape unrecognized; not safely scope-checkable".into(),
                offending_paths: Vec::new(),
            });
        }

        // Step 7: extract file paths from the command.
        let paths = Self::extract_paths(cmd);
        let real_paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|p| !Self::is_path_traversal(p))
            .collect();

        // Step 8: NEW branch — implicit-cwd write with no explicit path
        // is allowed (e.g. `cargo build` writes to ./target/, which we
        // assume is in scope when the agent's cwd is).
        if real_paths.is_empty() && implicit_cwd {
            return Ok(());
        }

        // Step 9: explicit write with no path is still an error.
        if real_paths.is_empty() {
            return Err(ScopeViolation {
                tool: tool.into(),
                reason: "write target not extractable; use a literal path".into(),
                offending_paths: Vec::new(),
            });
        }

        // Step 10: check each path against the allowed set.
        let mut offending: Vec<PathBuf> = Vec::new();
        for p in &real_paths {
            if !self.path_is_allowed(p) {
                offending.push(p.clone());
            }
        }
        if !offending.is_empty() {
            return Err(ScopeViolation {
                tool: tool.into(),
                reason: format!(
                    "{} path(s) outside allowed scope: {:?}",
                    offending.len(),
                    offending
                ),
                offending_paths: offending,
            });
        }
        Ok(())
    }

    /// True if the command contains any write indicator.
    fn detect_write_intent(cmd: &str) -> bool {
        // Redirect operators
        if cmd.contains('>') {
            return true;
        }
        // Write command verbs (first whitespace-separated token)
        let first = cmd.split_whitespace().next().unwrap_or("");
        const WRITE_VERBS: &[&str] = &[
            "rm", "mv", "cp", "sed", "install", "tee", "dd",
            "chmod", "chown", "ln", "touch", "mkdir", "rmdir",
        ];
        WRITE_VERBS.contains(&first)
    }

    /// True if the command contains a shell operator that combines commands.
    fn has_compound_operator(cmd: &str) -> bool {
        const SUSPICIOUS: &[&str] = &["||", "&&", ";", "$(", "`", "&"];
        SUSPICIOUS.iter().any(|op| cmd.contains(op))
    }

    /// Extract every `/`-led path-like token from the command. Manual
    /// character-class match — avoids pulling in the `regex` crate.
    fn extract_paths(cmd: &str) -> Vec<PathBuf> {
        let bytes = cmd.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'/' {
                let start = i;
                while i < bytes.len() {
                    let b = bytes[i];
                    if b.is_ascii_alphanumeric() || b == b'/' || b == b'.'
                        || b == b'_' || b == b'-'
                    {
                        i += 1;
                    } else {
                        break;
                    }
                }
                if i > start + 1 {
                    if let Ok(s) = std::str::from_utf8(&bytes[start..i]) {
                        out.push(PathBuf::from(s));
                    }
                }
            } else {
                i += 1;
            }
        }
        out
    }

    /// True if a path contains `..` segments after resolution. This
    /// blocks obvious traversal attempts.
    fn is_path_traversal(p: &Path) -> bool {
        p.components().any(|c| matches!(c, std::path::Component::ParentDir))
    }

    /// True if the first whitespace-separated token of `cmd` is in
    /// `self.implicit_cwd_verbs`. Instance method — uses the runtime-
    /// configured list.
    fn is_implicit_cwd_write_verb(&self, cmd: &str) -> bool {
        let first = cmd.split_whitespace().next().unwrap_or("");
        self.implicit_cwd_verbs.iter().any(|v| v == first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Node;

    fn guard(paths: &[&str]) -> ScopeGuard {
        ScopeGuard::new(paths.iter().map(PathBuf::from).collect())
    }

    #[test]
    fn new_stores_paths() {
        let g = guard(&["/abs/src", "/abs/config"]);
        assert_eq!(g.allowed_write_paths.len(), 2);
        assert!(!g.restrict_reads);
    }

    #[test]
    fn restrict_reads_toggles() {
        let g = guard(&["/abs"]).restrict_reads(true);
        assert!(g.restrict_reads);
    }

    #[test]
    fn path_is_allowed_within_prefix() {
        let g = guard(&["/abs/src"]);
        assert!(g.path_is_allowed(Path::new("/abs/src/main.rs")));
        assert!(g.path_is_allowed(Path::new("/abs/src/foo/bar.rs")));
        assert!(!g.path_is_allowed(Path::new("/abs/other/main.rs")));
        assert!(!g.path_is_allowed(Path::new("/etc/passwd")));
    }

    #[test]
    fn is_active_true_when_paths_present() {
        let g = guard(&["/abs"]);
        assert!(g.is_active());
        let empty = ScopeGuard::new(Vec::new());
        assert!(!empty.is_active());
    }

    #[test]
    fn from_involved_nodes_collects_file_paths() {
        let mut g = Graph::new();
        g.add_node(Node::file("/proj/src/a.rs", "A"));
        g.add_node(Node::file("/proj/src/b.rs", "B"));
        let nodes = vec![NodeId::from("/proj/src/a.rs"), NodeId::from("/proj/src/b.rs")];
        let scope = ScopeGuard::from_involved_nodes(&g, &nodes);
        assert_eq!(scope.allowed_write_paths.len(), 2);
        assert!(scope.path_is_allowed(Path::new("/proj/src/a.rs")));
        assert!(scope.path_is_allowed(Path::new("/proj/src/b.rs")));
        // The check is *prefix*, not parent — a different file under the
        // same directory is NOT in scope just because the directory
        // happens to contain an involved node.
        assert!(!scope.path_is_allowed(Path::new("/proj/src/sub/c.rs")));
    }

    #[test]
    fn from_involved_nodes_skips_task_nodes() {
        let mut g = Graph::new();
        g.add_node(Node::task("t1", "do something"));
        g.add_node(Node::file("/proj/src/a.rs", "A"));
        let nodes = vec![NodeId::from("t1"), NodeId::from("/proj/src/a.rs")];
        let scope = ScopeGuard::from_involved_nodes(&g, &nodes);
        assert_eq!(scope.allowed_write_paths.len(), 1);
        assert_eq!(scope.allowed_write_paths[0], PathBuf::from("/proj/src/a.rs"));
    }

    #[test]
    fn check_passes_through_non_bash_tools() {
        let g = guard(&["/abs"]);
        let input = serde_json::json!({"path": "/elsewhere/file.txt"});
        // Non-bash tools are permissive in v1; the check is no-op.
        assert!(g.check("edit_file", &input).is_ok());
    }

    // -- bash check tests --

    #[test]
    fn check_allows_read_only_commands() {
        let g = guard(&["/proj/src"]);
        for cmd in ["ls -la", "cat /proj/src/a.rs",
                    "grep TODO /proj/src/a.rs",
                    "git status", "cargo check"] {
            let input = serde_json::json!({"command": cmd});
            assert!(g.check("bash", &input).is_ok(), "should allow: {cmd}");
        }
    }

    #[test]
    fn check_allows_write_inside_scope() {
        let g = guard(&["/proj/src"]);
        let input = serde_json::json!({"command": "rm /proj/src/old.rs"});
        assert!(g.check("bash", &input).is_ok());
    }

    #[test]
    fn check_denies_write_outside_scope() {
        let g = guard(&["/proj/src"]);
        let input = serde_json::json!({"command": "rm /etc/passwd"});
        let err = g.check("bash", &input).unwrap_err();
        assert!(err.reason.contains("outside allowed scope"));
        assert_eq!(err.offending_paths, vec![PathBuf::from("/etc/passwd")]);
    }

    #[test]
    fn check_denies_path_traversal() {
        let g = guard(&["/proj/src"]);
        let input = serde_json::json!({"command": "rm /proj/../etc/passwd"});
        let err = g.check("bash", &input).unwrap_err();
        // Path traversal: extract_paths yields the raw /proj/../etc/passwd;
        // is_path_traversal filters it, leaving no real paths, so we hit
        // the "write target not extractable" reason.
        assert!(err.reason.contains("not extractable")
             || err.reason.contains("outside allowed scope"));
    }

    #[test]
    fn check_denies_complex_compound_commands() {
        let g = guard(&["/proj/src"]);
        let input = serde_json::json!({"command": "rm /proj/src/a && echo done"});
        let err = g.check("bash", &input).unwrap_err();
        assert!(err.reason.contains("too complex"));
    }

    #[test]
    fn check_denies_unrecognized_command_shape() {
        let g = guard(&["/proj/src"]);
        // `someobscurecommand` is not a write verb and has no redirect;
        // BashTool::classify_read_only will say false because it isn't
        // in the read-only list either. The scope guard then denies.
        let input = serde_json::json!({"command": "someobscurecommand"});
        let err = g.check("bash", &input).unwrap_err();
        assert!(err.reason.contains("unrecognized")
             || err.reason.contains("not safely"));
    }

    #[test]
    fn check_denies_redirect_outside_scope() {
        let g = guard(&["/proj/src"]);
        let input = serde_json::json!({"command": "echo x > /etc/evil.conf"});
        let err = g.check("bash", &input).unwrap_err();
        assert!(err.reason.contains("outside allowed scope"));
        assert!(err.offending_paths.contains(&PathBuf::from("/etc/evil.conf")));
    }

    #[test]
    fn check_extract_paths_simple() {
        let paths = ScopeGuard::extract_paths("cat /a/b.txt | grep foo > /c/d.txt");
        // /a/b.txt, /c/d.txt — /a/b.txt appears once.
        assert!(paths.contains(&PathBuf::from("/a/b.txt")));
        assert!(paths.contains(&PathBuf::from("/c/d.txt")));
    }

    #[test]
    fn check_extract_paths_ignores_short_matches() {
        // A single `/` alone is a 1-char match; we require >1 char.
        let paths = ScopeGuard::extract_paths("echo a / b");
        // `/` is 1 char; the extractor should not include it.
        assert!(!paths.contains(&PathBuf::from("/")));
    }

    #[test]
    fn check_detect_write_intent_redirect() {
        assert!(ScopeGuard::detect_write_intent("echo x > /tmp/out"));
        assert!(ScopeGuard::detect_write_intent("cmd >> /tmp/out"));
    }

    #[test]
    fn check_detect_write_intent_verbs() {
        assert!(ScopeGuard::detect_write_intent("rm /tmp/x"));
        assert!(ScopeGuard::detect_write_intent("mv a b"));
        assert!(ScopeGuard::detect_write_intent("sed -i 's/a/b/' file"));
        assert!(ScopeGuard::detect_write_intent("mkdir -p /tmp/x"));
        // Non-write verb → false
        assert!(!ScopeGuard::detect_write_intent("ls -la"));
        assert!(!ScopeGuard::detect_write_intent("cat /tmp/x"));
    }

    #[test]
    fn check_has_compound_operator() {
        assert!(ScopeGuard::has_compound_operator("a && b"));
        assert!(ScopeGuard::has_compound_operator("a || b"));
        assert!(ScopeGuard::has_compound_operator("a ; b"));
        assert!(ScopeGuard::has_compound_operator("a $(b)"));
        assert!(ScopeGuard::has_compound_operator("a `b`"));
        assert!(ScopeGuard::has_compound_operator("a &"));
        assert!(!ScopeGuard::has_compound_operator("a"));
        assert!(!ScopeGuard::has_compound_operator("a -la"));
    }

    #[test]
    fn check_restrict_reads_blocks_out_of_scope_read() {
        let g = guard(&["/proj/src"]).restrict_reads(true);
        let input = serde_json::json!({"command": "cat /etc/passwd"});
        let err = g.check("bash", &input).unwrap_err();
        assert!(
            err.reason.contains("outside allowed scope")
                || err.reason.contains("read path"),
            "reason was: {}",
            err.reason
        );
        assert!(err.offending_paths.contains(&PathBuf::from("/etc/passwd")));
    }

    #[test]
    fn check_restrict_reads_allows_in_scope_read() {
        let g = guard(&["/proj/src"]).restrict_reads(true);
        let input = serde_json::json!({"command": "cat /proj/src/a.rs"});
        assert!(g.check("bash", &input).is_ok());
    }

    #[test]
    fn scope_guard_default_implicit_cwd_verbs_has_expected_count() {
        let v = ScopeGuard::default_implicit_cwd_verbs();
        assert!(v.len() >= 12, "default should have at least 12 entries, got {}", v.len());
        // Sanity-check the most common ones are present
        for required in &["cargo", "npm", "pip", "make", "go", "python3"] {
            assert!(v.iter().any(|x| x == required),
                "default should include {required}, got: {:?}", v);
        }
        // No empty strings
        for entry in &v {
            assert!(!entry.is_empty(), "default verb list has empty entry");
        }
    }

    #[test]
    fn scope_guard_new_initializes_implicit_cwd_verbs_to_default() {
        let g = ScopeGuard::new(vec![]);
        let defaults = ScopeGuard::default_implicit_cwd_verbs();
        assert_eq!(g.implicit_cwd_verbs, defaults,
            "new() should initialize implicit_cwd_verbs from default_implicit_cwd_verbs()");
    }

    #[test]
    fn with_implicit_cwd_verb_adds_and_is_idempotent() {
        let g = ScopeGuard::new(vec![]).with_implicit_cwd_verb("cmake");
        assert!(g.implicit_cwd_verbs.contains(&"cmake".to_string()));
        // Calling again does not duplicate.
        let g2 = g.clone().with_implicit_cwd_verb("cmake");
        let count = g2.implicit_cwd_verbs.iter()
            .filter(|v| v == &"cmake").count();
        assert_eq!(count, 1, "with_implicit_cwd_verb should be idempotent");
    }

    #[test]
    fn without_implicit_cwd_verb_removes() {
        // "pip" is in the default list; removing it should take it out.
        let g = ScopeGuard::new(vec![]).without_implicit_cwd_verb("pip");
        assert!(!g.implicit_cwd_verbs.contains(&"pip".to_string()));
        // Other defaults are still present.
        assert!(g.implicit_cwd_verbs.contains(&"cargo".to_string()));
    }

    #[test]
    fn reset_implicit_cwd_verbs_restores_default() {
        // After several mutations, reset brings us back.
        let g = ScopeGuard::new(vec![])
            .with_implicit_cwd_verb("cmake")
            .with_implicit_cwd_verb("gradle")
            .without_implicit_cwd_verb("pip")
            .without_implicit_cwd_verb("cargo")
            .reset_implicit_cwd_verbs();
        let defaults = ScopeGuard::default_implicit_cwd_verbs();
        assert_eq!(g.implicit_cwd_verbs, defaults);
    }

    #[test]
    fn is_implicit_cwd_write_verb_uses_runtime_field() {
        // Default list includes "cargo".
        let g = ScopeGuard::new(vec![]);
        assert!(g.is_implicit_cwd_write_verb("cargo build"));
        assert!(g.is_implicit_cwd_write_verb("cargo"));
        // Not in default list.
        assert!(!g.is_implicit_cwd_write_verb("cmake build"));
        assert!(!g.is_implicit_cwd_write_verb("ls"));
        // After with_implicit_cwd_verb, the new verb is recognized.
        let g2 = g.clone().with_implicit_cwd_verb("cmake");
        assert!(g2.is_implicit_cwd_write_verb("cmake build"));
        // After without, the removed verb is no longer recognized.
        let g3 = g.without_implicit_cwd_verb("cargo");
        assert!(!g3.is_implicit_cwd_write_verb("cargo build"));
    }

    // -- Implicit cwd write behavioral tests (Task 6) --

    #[test]
    fn check_allows_cargo_build_in_scope() {
        let g = guard(&["/proj/src"]);
        let input = serde_json::json!({"command": "cargo build"});
        assert!(g.check("bash", &input).is_ok(),
            "cargo build (no explicit path) should be allowed as implicit cwd write");
    }

    #[test]
    fn check_allows_cargo_build_with_explicit_target_in_scope() {
        let g = guard(&["/proj/src"]);
        let input = serde_json::json!({"command": "cargo build --target-dir /proj/src/target"});
        assert!(g.check("bash", &input).is_ok(),
            "cargo build with in-scope --target-dir should be allowed");
    }

    #[test]
    fn check_denies_cargo_build_with_explicit_target_outside_scope() {
        let g = guard(&["/proj/src"]);
        let input = serde_json::json!({"command": "cargo build --target-dir /elsewhere"});
        let err = g.check("bash", &input).unwrap_err();
        assert!(err.reason.contains("outside allowed scope"),
            "reason should name the rule, got: {}", err.reason);
        assert!(err.offending_paths.contains(&PathBuf::from("/elsewhere")));
    }

    #[test]
    fn check_allows_npm_install_in_scope() {
        let g = guard(&["/proj/src"]);
        let input = serde_json::json!({"command": "npm install"});
        assert!(g.check("bash", &input).is_ok(),
            "npm install (no explicit path) should be allowed as implicit cwd write");
    }

    #[test]
    fn check_denies_pip_install_with_target_outside_scope() {
        let g = guard(&["/proj/src"]);
        let input = serde_json::json!({"command": "pip install --target /elsewhere foo"});
        let err = g.check("bash", &input).unwrap_err();
        assert!(err.reason.contains("outside allowed scope"),
            "reason should name the rule, got: {}", err.reason);
        assert!(err.offending_paths.contains(&PathBuf::from("/elsewhere")));
    }

    #[test]
    fn check_compound_command_denied_before_unrecognized() {
        // The reorder: compound check fires before "unrecognized" check.
        // An unknown command with `&&` should be "too complex", not "unrecognized".
        let g = guard(&["/proj/src"]);
        let input = serde_json::json!({"command": "someobscurecommand && echo done"});
        let err = g.check("bash", &input).unwrap_err();
        assert!(err.reason.contains("too complex"),
            "compound command should be 'too complex' (reordered), got: {}", err.reason);
    }
}
