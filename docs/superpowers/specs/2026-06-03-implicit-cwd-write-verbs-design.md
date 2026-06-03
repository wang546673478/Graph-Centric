# IMPLICIT_CWD_WRITE_VERBS — Build Tool Support in `ScopeGuard`

**Date:** 2026-06-03
**Status:** Approved (pending spec review)
**Scope:** v1.1 of the ScopeGuard bash check algorithm
**Parent spec:** `2026-06-03-tool-system-rework-design.md` §6.2 (the 7-step bash algorithm)

## 1. Context

The current `ScopeGuard::check_bash` algorithm (delivered in 2026-06-03) treats commands that aren't read-only AND aren't file-level write verbs (`rm, mv, cp, sed, install, tee, dd, chmod, chown, ln, touch, mkdir, rmdir`) as "command shape unrecognized" and denies them. This is correct conservative behavior for unknown commands, but it has an unintended consequence: **common build tools** (`cargo`, `npm`, `pip`, `make`, `go`, `python`, `node`, `rustc`, etc.) are also denied, because they aren't in the write-verb list and they have no `--target-dir`-style path argument by default.

In practice, build tools write to **cwd-based implicit locations**: `target/`, `node_modules/`, `dist/`, `__pycache__/`, etc. If the agent's cwd is within `ScopeGuard::allowed_write_paths`, these writes are legitimate. The harness should allow them; the current implementation denies them.

## 2. Goal

Allow the model to invoke common build tools inside a scope, when the tools have no explicit out-of-scope path argument. Continue to deny:

- Out-of-scope writes (any tool, any path)
- Explicit out-of-scope `--target-dir` style arguments
- Commands that are clearly dangerous (already covered by `DangerousCommandDeny`)

## 3. Non-goals (YAGNI)

- ❌ **No** precise "this command writes to `<subdir>`" detection. v1.1 trusts that build tools write to a cwd-based subdir and that the cwd is in scope.
- ❌ **No** parsing of subcommand semantics beyond the first whitespace-separated verb token. `cargo install`, `pip install`, `npm install -g` are NOT specially recognized as system-installs — they fall under the same rule as `cargo build`. (See §6 for the known limitation.)
- ❌ **No** new deny-list patterns for build tools. (The existing `DangerousCommandDeny` patterns already catch `cargo install` if it's a substring match — but `cargo install foo` does NOT match the `terraform-destroy` pattern and is currently allowed by the deny-list. We are not changing the deny-list in this spec.)
- ❌ **No** changes to `DangerousCommandDeny`, `SubAgent`, `SubAgentPool`, or any non-`scope_guard` module.
- ❌ **No** new file — all changes in `src/tools/scope_guard.rs`.

## 4. Design

### 4.1 Runtime-configurable verb list

The build-verb set is **not** a hardcoded const. It is a runtime field on `ScopeGuard` initialized to a sensible default, with builder methods for extension and reduction. The agent (or any caller) can ask the model to recommend additions/removals based on the project's tooling.

Add to `src/tools/scope_guard.rs`:

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

### 4.2 `ScopeGuard` struct update

Add the new field to the `ScopeGuard` struct:

```rust
pub struct ScopeGuard {
    pub allowed_write_paths: Vec<PathBuf>,
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

Update `new`:

```rust
pub fn new(allowed_write_paths: Vec<PathBuf>) -> Self {
    Self {
        allowed_write_paths,
        restrict_reads: false,
        implicit_cwd_verbs: default_implicit_cwd_verbs(),
    }
}
```

Add three builders (placed next to `restrict_reads`):

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

### 4.3 New private helper (uses field, not const)

```rust
/// True if the first whitespace-separated token of `cmd` is in
/// `self.implicit_cwd_verbs`. Instance method — uses the runtime-
/// configured list.
fn is_implicit_cwd_write_verb(&self, cmd: &str) -> bool {
    let first = cmd.split_whitespace().next().unwrap_or("");
    self.implicit_cwd_verbs.iter().any(|v| v == first)
}
```

### 4.4 Algorithm change in `check_bash`

The 7-step algorithm from the parent spec (§6.2) is extended with:

**New detection step (between current Steps 2 and 3):** detect `implicit_cwd = self.is_implicit_cwd_write_verb(cmd)`. The flag is computed alongside `write_intent`. The two are not mutually exclusive (e.g., `cargo build --target-dir /elsewhere` is BOTH implicit-cwd AND has an explicit path).

**Step reorder (per user direction 2026-06-03):** the compound-operator check moves UP to run **before** the "unrecognized" check. Rationale: a compound command is structurally unanalyzable, so we should refuse it on structural grounds before claiming we don't recognize the verb.

**Step 6 modified:** "write target not extractable" error is now only raised when `!implicit_cwd`. When `implicit_cwd == true` and no explicit paths are extracted, return `Ok(())`.

**Steps 4, 5, 7 (unchanged in content):** compound-operator check (moved position), path extraction, scope check still apply when paths ARE present.

### 4.5 Full new decision procedure (renumbered for clarity)

The `check_bash` body becomes:

1. Parse `input.command` as a string.
2. If `BashTool::classify_read_only(cmd) == true`:
   - If `restrict_reads == false` → return Ok(()). (Unchanged.)
   - Else: extract paths, check each; deny if any out of scope. (Unchanged from parent spec.)
3. Detect `write_intent` (redirect OR file-verb match). (Unchanged.)
4. Detect `implicit_cwd = self.is_implicit_cwd_write_verb(cmd)`. (New.)
5. **If `has_compound_operator(cmd)`** → Err "command shape too complex to scope-check; split into separate calls". (Was Step 4 in parent spec; **moved up** per user direction.)
6. **If `!write_intent && !implicit_cwd`** → Err "command shape unrecognized; not safely scope-checkable". (Was Step 3 in parent spec; **moved down** per user direction.)
7. Extract file paths from the command; filter out `..` traversal. (Unchanged.)
8. **If `real_paths.is_empty() && implicit_cwd`** → return Ok(()). (New branch — replaces the old "not extractable" error for this case.)
9. **Else if `real_paths.is_empty()`** → Err "write target not extractable; use a literal path". (Unchanged from parent spec.)
10. For each `real_paths` entry, resolve to absolute and check `path_is_allowed`. If any path is outside, Err "X path(s) outside allowed scope". (Unchanged.)

The change is bounded to:
- Step 4 (new detection)
- Step 5/6 reorder (compound before "unrecognized")
- Step 8 (new branch)

All other steps are unchanged.

## 5. Behavioral matrix

For a guard with `allowed_write_paths = ["/proj/src"]`:

| Command | `write_intent` | `implicit_cwd` | paths | Result |
|---|---|---|---|---|
| `ls` | false (read-only) | false | n/a (Step 2) | Ok |
| `cat /proj/src/a.rs` | false (read-only) | false | n/a (Step 2) | Ok |
| `rm /proj/src/old.rs` | true | false | `/proj/src/old.rs` | Ok (in scope) |
| `rm /etc/passwd` | true | false | `/etc/passwd` | Err "outside allowed scope" |
| `cargo build` | false | **true** | empty | **Ok (NEW)** |
| `cargo build --target-dir /proj/src/target` | false | true | `/proj/src/target` | Ok (in scope) |
| `cargo build --target-dir /elsewhere` | false | true | `/elsewhere` | Err "outside allowed scope" |
| `cargo build && echo done` | false | true | (compound) | Err "too complex" (reordered) |
| `npm install` | false | **true** | empty | **Ok (NEW)** |
| `pip install foo` | false | **true** | empty | **Ok (known hole — see §6)** |
| `make all` | false | true | empty | **Ok (NEW)** |
| `go build` | false | true | empty | **Ok (NEW)** |
| `python script.py` | false | true | empty | **Ok (NEW)** |
| `someobscurecommand` | false | false | n/a | Err "unrecognized" (unchanged) |
| `someobscurecommand && echo done` | false | false | (compound) | Err "too complex" (reordered — was "unrecognized" before reorder) |
| `rm /tmp/x && echo done` | true | false | (compound) | Err "too complex" (unchanged) |

## 6. Known limitations (documented, not addressed in v1.1)

These three limitations are part of the design's risk model. They are **disclosed in the README** so users know what the system does and does not protect against.

1. **System-install commands are allowed.** `cargo install foo`, `pip install foo`, `npm install -g foo` all fall under the implicit-cwd rule and are permitted. In practice these write to `~/.cargo/`, `site-packages/`, or global node_modules — which are typically NOT in the agent's allowed scope, but we cannot detect that without parsing the tool's behavior. **Mitigation:** the user (or dispatcher config) can call `ScopeGuard::without_implicit_cwd_verb("cargo")` (or `pip`, `npm`) to remove the verb from the default set for stricter environments. Future work: per-subcommand exclusion list (e.g., `["cargo install", "pip install", "npm install -g"]` → deny).

2. **Build tool detection is by first token only.** A malicious command that aliases to a build tool (e.g., `cargo` shadowed by a shell function writing to `/etc/`) would pass the verb check but then trip `DangerousCommandDeny` if it actually does something dangerous. The two guards compose correctly: a build-verb command with an explicit out-of-scope path is denied by `ScopeGuard`; a build-verb command with a destructive payload is denied by `DangerousCommandDeny`. The runtime-configurable verb list (via `with_implicit_cwd_verb` / `without_implicit_cwd_verb`) is the user-facing escape hatch for project-specific tools.

3. **`cargo run`, `cargo test`, `cargo bench` are all allowed.** These write to `target/` and may execute arbitrary code. The `DangerousCommandDeny` deny-list does not catch them because there's no substring match. **v1.1 accepts this risk** because: (a) the user is using build tools, (b) the agent's cwd is in scope, (c) the user trusts the model enough to run build commands. **Mitigation:** users can call `ScopeGuard::without_implicit_cwd_verb("cargo")` to disable all cargo invocations, or layer a custom `Policy` that catches `cargo test` / `cargo run` by substring match.

## 7. Tests

### 7.1 New unit tests for the runtime-configurable verb list (4 tests)

1. **`scope_guard_default_implicit_cwd_verbs_has_expected_count`** — `default_implicit_cwd_verbs()` returns the 12 default tools. Asserts at least 12 entries; sanity-checks that `cargo`, `npm`, `pip` are in the list.
2. **`with_implicit_cwd_verb_adds_and_is_idempotent`** — `ScopeGuard::new(vec![]).with_implicit_cwd_verb("cmake")` adds `cmake`; calling again does not duplicate.
3. **`without_implicit_cwd_verb_removes`** — `ScopeGuard::new(vec![]).without_implicit_cwd_verb("pip")` removes `pip` from the default list.
4. **`reset_implicit_cwd_verbs_restores_default`** — After several `with_/without_` calls, `reset_implicit_cwd_verbs` brings the list back to the default 12.

### 7.2 New unit tests for the behavioral change in `check_bash` (6 tests)

5. **`check_allows_cargo_build_in_scope`** — guard `[/proj/src]`, command `cargo build`, expect Ok.
6. **`check_allows_cargo_build_with_explicit_target_in_scope`** — guard `[/proj/src]`, command `cargo build --target-dir /proj/src/target`, expect Ok.
7. **`check_denies_cargo_build_with_explicit_target_outside_scope`** — guard `[/proj/src]`, command `cargo build --target-dir /elsewhere`, expect Err with "outside allowed scope" in reason and `/elsewhere` in `offending_paths`.
8. **`check_allows_npm_install_in_scope`** — guard `[/proj/src]`, command `npm install`, expect Ok.
9. **`check_denies_pip_install_with_target_outside_scope`** — guard `[/proj/src]`, command `pip install --target /elsewhere foo`, expect Err.
10. **`check_compound_command_denied_before_unrecognized`** — guard `[/proj/src]`, command `someobscurecommand && echo done`, expect Err with "too complex" (not "unrecognized"). This pins the step reorder.

### 7.3 Existing tests unchanged

All 21 pre-existing `scope_guard` tests must continue to pass. The behavioral change is additive; existing behavior on read-only, file-verb write, redirect, restrict_reads, traversal, "not extractable" is preserved.

### 7.4 No new integration tests

The existing 3 integration tests in `tests/integration_tool_guards.rs` continue to cover the high-level flow. A new integration test that exercises `cargo build` would require a real Cargo project in the test fixture — out of scope for v1.1.

## 8. Module doc update

Add a paragraph to the module-level doc comment of `src/tools/scope_guard.rs`:

```
/// ## Build tool support
///
/// Common build tools (`cargo`, `npm`, `pip`, `make`, `go`, `python`,
/// `node`, `rustc`, etc.) are recognized as "implicit cwd writes":
/// they are treated as write intent for scope checking, but when no
/// explicit path is given in the command, the scope check is skipped
/// (we assume the tool writes to a cwd-based subdirectory like
/// `target/`, `node_modules/`, `dist/`, etc., which is in scope when
/// the agent's cwd is).
///
/// The default verb list is `default_implicit_cwd_verbs()` and is
/// runtime-configurable via `with_implicit_cwd_verb` /
/// `without_implicit_cwd_verb` / `reset_implicit_cwd_verbs`.
/// Project-specific tools (cmake, gradle, mvn, bundle, gem, etc.)
/// can be added; tools known to write outside cwd (e.g., `pip` in
/// projects using virtualenvs) can be removed.
///
/// **Known v1.1 limitations** (also disclosed in README): system-install
/// commands like `cargo install`, `pip install`, `npm install -g` are
/// allowed by this rule (they write outside cwd but we can't tell).
/// Stricter environments can remove the corresponding verb from
/// `implicit_cwd_verbs` via `without_implicit_cwd_verb`.
```

## 9. README update (per user direction 2026-06-03)

Per user direction, the three known limitations from §6 must be **disclosed in the README** so users know what the system does and does not protect against.

Add a new subsection under "Honest scope" titled **"Build tool caveats"**:

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

This disclosure goes in the README's "Honest scope" section (per §6's title pattern), not in the module doc (which is for implementers).

## 10. Files

- Modify: `src/tools/scope_guard.rs` (add field + 3 builders + helper, modify `check_bash`, add 10 tests, update module doc)
- Modify: `README.md` (add the "Build tool caveats" subsection under "Honest scope")
- No other files changed.

## 11. Acceptance criteria

- [ ] `default_implicit_cwd_verbs()` returns the 12 default tools listed in §4.1
- [ ] `ScopeGuard` struct has `implicit_cwd_verbs: Vec<String>` field
- [ ] `with_implicit_cwd_verb`, `without_implicit_cwd_verb`, `reset_implicit_cwd_verbs` builders exist and work as tested in §7.1
- [ ] `is_implicit_cwd_write_verb` is an instance method using `self.implicit_cwd_verbs`
- [ ] `check_bash` algorithm matches §4.5 (new step 4 detection, reordered step 5/6, new step 8 branch)
- [ ] All 10 new tests in §7.1-§7.2 pass
- [ ] All 21 pre-existing `scope_guard` tests continue to pass
- [ ] All 298 pre-existing project tests continue to pass
- [ ] Module-level doc comment is updated per §8
- [ ] README has the "Build tool caveats" subsection per §9
- [ ] `cargo check -p graph_harness --tests` is clean (no warnings)
- [ ] `cargo run --bin demo` runs to completion (no new failures)

## 12. Out-of-scope follow-ups (for future spec)

- v1.2: per-verb subcommand exclusion list (deny `cargo install`, `pip install`, `npm install -g`)
- v1.2: precise "this command writes to `<subdir>`" detection via per-tool metadata
- v1.2: per-verb arg-flavor parsing (e.g., `--target-dir`, `--prefix`, `--out-dir`)
- v2.0: separate "build tool" tool abstraction (not just a verb in bash)
