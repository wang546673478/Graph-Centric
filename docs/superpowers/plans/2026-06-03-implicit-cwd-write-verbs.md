# IMPLICIT_CWD_WRITE_VERBS Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make common build tools (`cargo`, `npm`, `pip`, `make`, `go`, `python`, `node`, `rustc`) work inside a `ScopeGuard`-restricted sub-agent by recognizing them as "implicit cwd writes" — when the command has no explicit out-of-scope path argument, the scope check is skipped. The verb list is runtime-configurable.

**Architecture:** Add a new `implicit_cwd_verbs: Vec<String>` field to `ScopeGuard`, initialized from a new `default_implicit_cwd_verbs()` function. Add 3 builder methods (`with_implicit_cwd_verb`, `without_implicit_cwd_verb`, `reset_implicit_cwd_verbs`). Convert the existing free-function `is_implicit_cwd_write_verb` into an instance method using the field. Modify `check_bash` to: (a) detect `implicit_cwd` alongside `write_intent`; (b) reorder so the compound-operator check runs before the "unrecognized" check; (c) allow implicit-cwd writes with no explicit path. Update the module doc and the README's "Honest scope" section.

**Tech Stack:** Rust 2024 edition, `serde`, `tokio`, no new external dependencies.

**Spec:** `docs/superpowers/specs/2026-06-03-implicit-cwd-write-verbs-design.md`
**Parent spec:** `docs/superpowers/specs/2026-06-03-tool-system-rework-design.md`

**Note on git:** This project does not currently have a git repository. Where the template shows `git commit` as a step, instead run `cargo check` (or `cargo test` for test tasks) to verify the change compiles and behaves correctly. The "checkpoint" idea still applies — verify state at each task boundary.

---

## File Structure

**Modified files:**
- `src/tools/scope_guard.rs` — add field, builders, helper, modify `check_bash`, add 10 tests, update module doc
- `README.md` — add "Build tool caveats" subsection under "Honest scope"

**No new files created.**

---

## Task 1: Add `default_implicit_cwd_verbs()` function with tests

**Files:**
- Modify: `src/tools/scope_guard.rs` (add function and 1 test)

- [ ] **Step 1: Add the function and a test**

Add the function just below the existing `ScopeGuard::is_active` method (around line 153 in the current file — read the file to find the right spot):

```rust
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
        // JavaScript / Node
        "go".into(), "node".into(), "npm".into(), "yarn".into(), "pnpm".into(),
        // Python
        "python".into(), "python3".into(), "pip".into(), "pip3".into(),
        // Generic build
        "make".into(),
    ]
}
```

Add this test to the `mod tests` block in the same file:

```rust
    #[test]
    fn scope_guard_default_implicit_cwd_verbs_has_expected_count() {
        let v = default_implicit_cwd_verbs();
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
```

- [ ] **Step 2: Run the new test**

Run: `cargo test -p graph_harness tools::scope_guard::tests::scope_guard_default_implicit_cwd_verbs_has_expected_count`
Expected: PASS.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test -p graph_harness`
Expected: All 298 pre-existing tests still pass; the new test brings the count to 299.

---

## Task 2: Add `implicit_cwd_verbs` field to `ScopeGuard` struct and initialize in `new`

**Files:**
- Modify: `src/tools/scope_guard.rs` (add field, update `new`, add 1 test)

- [ ] **Step 1: Add the field to the `ScopeGuard` struct**

Find the `ScopeGuard` struct definition (around line 24-31 of the current file) and add a third field after `restrict_reads`:

```rust
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
```

- [ ] **Step 2: Update `new` to initialize the field**

Find the existing `new` constructor:

```rust
    pub fn new(allowed_write_paths: Vec<PathBuf>) -> Self {
        Self {
            allowed_write_paths,
            restrict_reads: false,
        }
    }
```

Replace it with:

```rust
    pub fn new(allowed_write_paths: Vec<PathBuf>) -> Self {
        Self {
            allowed_write_paths,
            restrict_reads: false,
            implicit_cwd_verbs: default_implicit_cwd_verbs(),
        }
    }
```

- [ ] **Step 3: Add a test for the field's default value**

Add this test to the `mod tests` block:

```rust
    #[test]
    fn scope_guard_new_initializes_implicit_cwd_verbs_to_default() {
        let g = ScopeGuard::new(vec![]);
        let defaults = default_implicit_cwd_verbs();
        assert_eq!(g.implicit_cwd_verbs, defaults,
            "new() should initialize implicit_cwd_verbs from default_implicit_cwd_verbs()");
    }
```

- [ ] **Step 4: Run the new test and the full scope_guard suite**

Run: `cargo test -p graph_harness tools::scope_guard::tests`
Expected: All 22 scope_guard tests pass (21 pre-existing + 1 from Task 1 + 1 from this task, but check the count).

- [ ] **Step 5: Run the full test suite**

Run: `cargo test -p graph_harness`
Expected: All 300 tests pass (298 pre-existing + 2 from this plan).

---

## Task 3: Add the three builders (`with_implicit_cwd_verb`, `without_implicit_cwd_verb`, `reset_implicit_cwd_verbs`)

**Files:**
- Modify: `src/tools/scope_guard.rs` (add 3 builders + 3 tests)

- [ ] **Step 1: Add the three builders after `restrict_reads`**

Find the existing `restrict_reads` builder and add the three new builders right after it:

```rust
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
        self.implicit_cwd_verbs = default_implicit_cwd_verbs();
        self
    }
```

- [ ] **Step 2: Add 3 tests for the builders**

Add these to the `mod tests` block:

```rust
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
        let defaults = default_implicit_cwd_verbs();
        assert_eq!(g.implicit_cwd_verbs, defaults);
    }
```

- [ ] **Step 3: Run the new tests**

Run: `cargo test -p graph_harness tools::scope_guard::tests::with_implicit_cwd_verb_adds_and_is_idempotent tools::scope_guard::tests::without_implicit_cwd_verb_removes tools::scope_guard::tests::reset_implicit_cwd_verbs_restores_default`
Expected: 3 passed.

- [ ] **Step 4: Run the full scope_guard suite**

Run: `cargo test -p graph_harness tools::scope_guard::tests`
Expected: All 25 scope_guard tests pass.

- [ ] **Step 5: Run the full project test suite**

Run: `cargo test -p graph_harness`
Expected: All 303 tests pass.

---

## Task 4: Convert `is_implicit_cwd_write_verb` to an instance method using the field

**Files:**
- Modify: `src/tools/scope_guard.rs` (convert helper from free function to instance method + add 1 test)

- [ ] **Step 1: Check if the helper already exists as a free function**

Search for `is_implicit_cwd_write_verb` in `src/tools/scope_guard.rs`. If it exists as a free function (returning bool from a `&str` argument), replace it. If it doesn't exist yet, just add the instance method.

**If it exists**, replace the free function:

```rust
/// True if the first whitespace-separated token of `cmd` is in
/// `self.implicit_cwd_verbs`. Instance method — uses the runtime-
/// configured list.
fn is_implicit_cwd_write_verb(&self, cmd: &str) -> bool {
    let first = cmd.split_whitespace().next().unwrap_or("");
    self.implicit_cwd_verbs.iter().any(|v| v == first)
}
```

**If it doesn't exist**, just add the instance method above.

- [ ] **Step 2: Add a test for the instance method**

Add this test to the `mod tests` block (the test verifies the instance method, using the field's runtime configuration):

```rust
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
        let g2 = g.with_implicit_cwd_verb("cmake");
        assert!(g2.is_implicit_cwd_write_verb("cmake build"));
        // After without, the removed verb is no longer recognized.
        let g3 = g.without_implicit_cwd_verb("cargo");
        assert!(!g3.is_implicit_cwd_write_verb("cargo build"));
    }
```

- [ ] **Step 3: Run the new test**

Run: `cargo test -p graph_harness tools::scope_guard::tests::is_implicit_cwd_write_verb_uses_runtime_field`
Expected: PASS.

- [ ] **Step 4: Run the full scope_guard suite and project suite**

Run: `cargo test -p graph_harness tools::scope_guard::tests`
Run: `cargo test -p graph_harness`
Expected: All 26 scope_guard tests pass; all 304 project tests pass.

---

## Task 5: Modify `check_bash` — add `implicit_cwd` detection, reorder compound/combined check, add step 8 branch

**Files:**
- Modify: `src/tools/scope_guard.rs` (modify `check_bash` algorithm)

This is the substantive behavioral change. The algorithm goes from 7 steps to 10 steps (with the new implicit_cwd detection, the compound/combined reorder, and the new "empty + implicit_cwd → Ok" branch). All other steps are unchanged in content.

- [ ] **Step 1: Find the current `check_bash` method**

The current `check_bash` is in `src/tools/scope_guard.rs` starting around line 152. Read it to confirm its current shape before modifying.

- [ ] **Step 2: Replace `check_bash` with the new version**

Find the entire `check_bash` method (everything from `pub fn check_bash(...)` through its closing `}`) and replace it with:

```rust
    /// The internal bash-specific check. Public to allow focused tests.
    pub fn check_bash(&self, cmd: &str, tool: &str) -> Result<(), ScopeViolation> {
        // Step 1: classify read vs write.
        let is_ro = crate::tools::BashTool::classify_read_only(cmd);
        if is_ro && !self.restrict_reads {
            return Ok(());
        }

        // Step 2: detect write intent (file-level mutation verb or redirect).
        let write_intent = Self::detect_write_intent(cmd);

        // Step 3: detect implicit-cwd write intent (build tool verb).
        let implicit_cwd = self.is_implicit_cwd_write_verb(cmd);

        // Step 4: compound operator check (moved UP from the parent spec's
        // step 4; runs before the "unrecognized" check per user direction).
        if Self::has_compound_operator(cmd) {
            return Err(ScopeViolation {
                tool: tool.into(),
                reason: "command shape too complex to scope-check; split into separate calls".into(),
                offending_paths: Vec::new(),
            });
        }

        // Step 5: combined "unrecognized" check.
        if !write_intent && !implicit_cwd {
            return Err(ScopeViolation {
                tool: tool.into(),
                reason: "command shape unrecognized; not safely scope-checkable".into(),
                offending_paths: Vec::new(),
            });
        }

        // Step 6: extract file paths from the command.
        let paths = Self::extract_paths(cmd);
        let real_paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|p| !Self::is_path_traversal(p))
            .collect();

        // Step 7: NEW branch — implicit-cwd write with no explicit path is allowed.
        if real_paths.is_empty() && implicit_cwd {
            return Ok(());
        }

        // Step 8: explicit write with no path is still an error.
        if real_paths.is_empty() {
            return Err(ScopeViolation {
                tool: tool.into(),
                reason: "write target not extractable; use a literal path".into(),
                offending_paths: Vec::new(),
            });
        }

        // Step 9: check each path against the allowed set.
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
```

- [ ] **Step 3: Run the existing scope_guard tests to verify no regression**

Run: `cargo test -p graph_harness tools::scope_guard::tests`
Expected: All 26 pre-existing tests still pass. The new behavioral tests (added in Task 6) are not yet present.

If a previously-passing test now fails, **STOP and report BLOCKED or DONE_WITH_CONCERNS** with the failing test name and the error.

- [ ] **Step 4: Run the full project suite**

Run: `cargo test -p graph_harness`
Expected: All 304 tests pass.

---

## Task 6: Add the 6 new behavioral tests for the modified `check_bash`

**Files:**
- Modify: `src/tools/scope_guard.rs` (add 6 tests to `mod tests`)

- [ ] **Step 1: Add the 6 new behavioral tests**

Add these to the `mod tests` block:

```rust
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
```

- [ ] **Step 2: Run the 6 new tests**

Run: `cargo test -p graph_harness tools::scope_guard::tests::check_allows_cargo_build_in_scope tools::scope_guard::tests::check_allows_cargo_build_with_explicit_target_in_scope tools::scope_guard::tests::check_denies_cargo_build_with_explicit_target_outside_scope tools::scope_guard::tests::check_allows_npm_install_in_scope tools::scope_guard::tests::check_denies_pip_install_with_target_outside_scope tools::scope_guard::tests::check_compound_command_denied_before_unrecognized`
Expected: 6 passed.

- [ ] **Step 3: Run the full scope_guard suite**

Run: `cargo test -p graph_harness tools::scope_guard::tests`
Expected: All 32 scope_guard tests pass (26 + 6 new).

- [ ] **Step 4: Run the full project suite**

Run: `cargo test -p graph_harness`
Expected: All 310 tests pass (304 + 6 new).

---

## Task 7: Update the module-level doc comment

**Files:**
- Modify: `src/tools/scope_guard.rs` (update module doc)

- [ ] **Step 1: Find the existing module doc comment**

The module doc is the `//!` block at the top of `src/tools/scope_guard.rs` (before the `use` statements). Read it to find its current shape.

- [ ] **Step 2: Append the "Build tool support" section**

Add this paragraph at the end of the module doc block (just before the closing `//! ` line, or after the last existing `//!` line):

```rust
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
```

- [ ] **Step 3: Verify the file still compiles and tests pass**

Run: `cargo check -p graph_harness --tests`
Run: `cargo test -p graph_harness`
Expected: 310 tests pass, no warnings.

---

## Task 8: Update README with "Build tool caveats" subsection

**Files:**
- Modify: `README.md` (add the "Build tool caveats" subsection)

- [ ] **Step 1: Find the "Honest scope" section in README**

Search for `## Honest scope` in `/home/hhhh/Graph-Centric/README.md`. The current section is around line 313 (it was updated in the previous plan).

- [ ] **Step 2: Add the "Build tool caveats" subsection**

Insert this subsection as the **first** item under "Honest scope" (before the "What this is NOT" list), or as a separate subsection **after** the "Honest scope" list. The plan recommends **after the list** for readability.

Find the end of the "What this is NOT (yet):" list (the last bullet is "A persistence layer..."). Insert after that, before the "## License" section.

Add this markdown:

```markdown
### Build tool caveats

The bash guard recognizes common build tools (`cargo`, `npm`, `pip`,
`make`, `go`, `python`, `node`, `rustc`) as "implicit cwd writes":
when the command has no explicit `--target-dir`-style argument, the
scope check is skipped (we assume the tool writes to a cwd-based
subdir like `target/` or `node_modules/`). This is configurable via
`ScopeGuard::with_implicit_cwd_verb` / `without_implicit_cwd_verb`.

**Three known v1.1 limitations:**

1. **System-install commands are allowed.** `cargo install foo`,
   `pip install foo`, `npm install -g foo` fall under the same
   rule and are permitted. They actually write to `~/.cargo/`,
   site-packages, or global node_modules — which are typically NOT
   in the agent's allowed scope. **Mitigation:** call
   `ScopeGuard::without_implicit_cwd_verb("cargo")` (or `pip`, `npm`)
   in dispatcher config for stricter environments.

2. **Build tool detection is by first token only.** A shell alias
   named `cargo` that writes to `/etc/` would pass the verb check.
   `DangerousCommandDeny` would catch destructive payloads; the
   scope check would catch explicit out-of-scope paths. Neither
   catches a clever alias. Trust the model accordingly.

3. **`cargo run`, `cargo test`, `cargo bench` are allowed.** They
   may execute arbitrary code. The deny-list does not catch them.
   **Mitigation:** call `ScopeGuard::without_implicit_cwd_verb("cargo")`
   to disable all cargo invocations, or layer a custom `Policy`.
```

Also update the test count in the README ("259" → "310") if the README still says 259. (The previous plan bumped it to 298; this plan will bring it to 310.)

- [ ] **Step 3: Verify the README renders correctly and the project still builds**

Run: `cargo test -p graph_harness`
Expected: 310 tests pass.

(There's no automated way to verify the markdown renders correctly; manually check by reading the file.)

---

## Self-Review

**1. Spec coverage:**

| Spec section | Plan task |
|---|---|
| §4.1 `default_implicit_cwd_verbs()` function | Task 1 |
| §4.2 `implicit_cwd_verbs` field + `new` initialization | Task 2 |
| §4.2 Three builders | Task 3 |
| §4.3 Instance method `is_implicit_cwd_write_verb` | Task 4 |
| §4.5 New `check_bash` algorithm (step reorder, new step 7 branch) | Task 5 |
| §7.1 Tests 1-4 (runtime-configurable verb list) | Tasks 2 + 3 + 4 |
| §7.2 Tests 5-10 (behavioral change) | Task 6 (test 5 is in Task 1, tests 6-10 are in Task 6) |
| §8 Module doc update | Task 7 |
| §9 README update with "Build tool caveats" | Task 8 |
| §11 Acceptance criteria | Verified by all tasks |

**2. Placeholder scan:** No "TBD" / "TODO" / "fill in details" in the plan. Every step has concrete code or specific instructions.

**3. Type consistency check:**

| Name | Defined in | Used in | Status |
|---|---|---|---|
| `default_implicit_cwd_verbs() -> Vec<String>` | Task 1 | Tasks 2, 3 | ✅ |
| `ScopeGuard::implicit_cwd_verbs: Vec<String>` | Task 2 | Tasks 3, 4, 5 | ✅ |
| `with_implicit_cwd_verb(self, impl Into<String>) -> Self` | Task 3 | Task 3 test, Task 4 test | ✅ |
| `without_implicit_cwd_verb(self, &str) -> Self` | Task 3 | Task 3 test, Task 4 test | ✅ |
| `reset_implicit_cwd_verbs(self) -> Self` | Task 3 | Task 3 test | ✅ |
| `is_implicit_cwd_write_verb(&self, &str) -> bool` | Task 4 | Task 5 (check_bash) | ✅ |
| `detect_write_intent(&str) -> bool` (existing) | existing | Task 5 | ✅ unchanged |
| `has_compound_operator(&str) -> bool` (existing) | existing | Task 5 | ✅ unchanged |
| `extract_paths(&str) -> Vec<PathBuf>` (existing) | existing | Task 5 | ✅ unchanged |
| `is_path_traversal(&Path) -> bool` (existing) | existing | Task 5 | ✅ unchanged |

**4. Ambiguity check:**

- Task 1: the function is added "just below the existing `ScopeGuard::is_active` method (around line 153)" — if the actual line is different, the engineer should find the right spot. The plan's self-review notes that Step 1 says "read the file to find the right spot".
- Task 4: the helper may or may not exist as a free function. The plan handles both cases.
- Task 5: the new `check_bash` is fully specified — every step has concrete code.
- Task 8: the README insertion point is described precisely (after the "Honest scope" list, before "## License").

**5. Scope check:** This plan is one self-contained change. It touches 2 files (1 Rust + 1 markdown). It is well within the bounds of a single implementation plan.
