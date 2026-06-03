# Architecture

This document explains the *why* behind the design — the decisions that
aren't obvious from reading the code, the alternatives we considered and
rejected, and the trade-offs that shape every component. Read `README.md`
first if you haven't.

---

## 1. Core thesis: binding-constraint on long agent tasks

Empirical work on long-running LLM agents (SWE-agent, Terminal-Bench, the
Anthropic orchestrator-workers writeup) converges on a single observation:
**once tasks get long, system performance is dominated by the harness
around the model, not the model's reasoning ability.** Holding the model
fixed, the harness alone moves benchmark scores by orders of magnitude.

Most harnesses today are some flavour of ReAct loop — "model thinks, model
calls a tool, model thinks again." This works for short, locally-scoped
tasks. It scales poorly because:

- **Context drifts.** Each tool result lands in the conversation as a
  message. After 20 turns the model is reasoning about its own past tool
  output rather than the underlying world.
- **Errors compound silently.** A small misunderstanding in turn 3 shapes
  every subsequent turn until it surfaces as a tangible failure 15 turns
  later. By then the proximate cause is buried.
- **Coordination is implicit.** Sub-agents, when present at all, share
  context via concatenated messages. They drift apart.

Our bet is that **a relationship graph as the shared substrate**
counteracts all three failure modes. The graph is the world model that
every component reads and writes; the conversation becomes commentary, not
state.

---

## 2. The three-layer graph (L0 / L1 / L2)

The graph is layered so structure, semantics, and raw content can each be
verified and revised independently.

### Layer separation

| Layer | What it is | Why separate |
|---|---|---|
| **L0** | nodes + edges (structure) | Cheap to scan, cheap to diff, cheap to ship between components |
| **L1** | per-node `{responsibility, implementation, design_intent, constraints}` + confidence | Semantic understanding is expensive to derive (model call per node) but small to carry; keeping it apart from L0 lets us version, validate, and re-enrich it |
| **L2** | raw bytes — source files, configs, schemas, datasets | Huge; never stored in the graph. Accessed on demand via `SourceLoader` |

### Why not just one layer?

We tried (mentally) collapsing into a single rich-node model with all
content inline. Two problems killed it:

1. **Cache busting.** Touching any field (say, a node's L1 description)
   would mean re-serializing the entire node — including its L2 payload.
   With distance-based context construction, that translates to huge
   re-renders every patch.
2. **Verification asymmetry.** L0 can be checked structurally with no
   model calls. L1 needs the model to sample-against-L2. L2 is itself
   ground truth. Mixing them obscures which check fires when.

### Why specifically these three?

L0 is forced — graphs are nodes + edges. L1 = "muscle" is the layer we
discovered during the v2 redesign: model output is otherwise scattered
across `Node.summary` (one-liner) and ad-hoc places, never structured.
Forcing L1 into a typed `{responsibility, implementation, design_intent,
constraints}` shape gave us:

- A target for the L1-sampling verifier ("does this drift from L2?")
- A repair entry point (`GraphError::L1Semantic` routes to re-enrichment)
- A meaningful unit for context compression by distance (full L1 at d=0,
  brief at d=1, oneline at d=2+)

L2 stays out of the graph because file content is volatile and large.
Reading it on demand keeps the graph small and forces the harness to
think about *when* L2 is actually needed (turns out: less often than you'd
think, once L1 is good).

### Cross-layer triggers

```
L0 patch adds nodes → auto-trigger L1Enricher on new nodes
L1 entry's confidence drifts low → enrichment re-runs
L2 mutates (sub-agent edits file) → ideally trigger L0 incremental scan + L1 refresh (Phase 5 — not yet)
```

The auto-trigger in `GraphLoop::auto_enrich` enforces the L0→L1 link.
L2→L0 is reactive: the next round's Verifier or PostExecutionValidator
notices the drift and surfaces a `GraphInvalid` for repair.

---

## 3. State machine: why a fixed FSM, not dynamic workflows

Claude Code uses **dynamic workflows** — the model writes Python-like
control flow that the harness executes. This is flexible but two things
made us pick a fixed state machine instead:

1. **Determinism of the spine.** With a fixed FSM, every transition has
   a single code path. When debugging, you read three `step_*` methods
   and you know exactly what can happen. With dynamic workflows, the
   model's runtime choices are the control flow, which means debugging
   the agent and debugging the model become the same problem.
2. **No model required for the spine.** The state machine runs without a
   model at all (see the `structural_only` verifier, `AlwaysPasses`
   validator, etc.). This is what makes tests fast and the trust
   boundary clear: the spine is verified Rust code; the leaves are
   model calls we tolerate uncertainty from.

The trade-off: less expressive. We can't have the model invent novel
multi-step coordination strategies mid-task. We bet that the cost of
losing that flexibility is smaller than the cost of losing
determinism on the spine.

### The three phases

| Phase | Owns | Exits to |
|---|---|---|
| Graph | Building / repairing the L0+L1 relationship graph | Task (verifier pass), self (repair), GraphInvalid (verifier stalemate) |
| Task | Decomposition + dispatching sub-agents | Review (all succeeded), GraphInvalid (sub-agent reports), GraphInvalid (PostExecutionValidator), TaskFailed (sub-agent code failures) |
| Review | Final acceptance gate | Done (pass), GraphInvalid (judge flags graph/scope), Done with embedded fail-verdict (judge flags task) |

These aren't sub-phases; each is a distinct beat of `step()`. The
caller-visible `LoopState` reflects this — `Running` ticks while the
machine is mid-phase, named states when something needs the caller.

### Why `step()` is reentrant

A `Paused { question }` or `GraphInvalid { errors }` is returned every
call to `step()` until the caller resolves it via `resume(...)` or
`resume_with_repaired_graph(...)`. Repeated calls don't advance the
machine; they re-surface the pending state. This makes the caller's
event loop trivial: call `step()` in a loop, dispatch on the variant,
forget about ordering or thread safety.

The alternative (a one-shot return + caller has to remember context)
would push state-machine awareness into every caller. Reentrant
surfacing keeps the contract narrow.

### Why GraphLoop is purely passive

`GraphLoop` never reads stdin, never opens a browser, never calls a
repairer on its own when surfacing `GraphInvalid` to the caller. All
external interaction happens through `resume_*`. This means:

- The same `GraphLoop` works for a CLI (Demo A), a web service, an
  automated CI harness, a test fixture, an LSP server, etc.
- Every external interaction is a discrete event with a payload that's
  obvious in the transcript.

The cost: the caller has more orchestration code (auto-repair loop in
Demo A is ~50 lines). The benefit: that orchestration is *visible* and
swappable. Phase 4 added Demo A's auto-repair without touching
`GraphLoop` itself.

---

## 4. Component separations

The names look redundant until you trace what each verifies and when:

### `Verifier` (Graph phase)

- **When**: every `ReadyForVerify` transition; also re-runs after each
  `LocalRepairer` patch.
- **What**: does the graph capture the task?
- **Layers**: structural (deterministic) + model self-check + L1 sampling.
- **Output**: `VerificationResult` with structured issues; high-severity
  blocks; medium/low surfaces.

### `LocalRepairer` (Graph phase, internal)

- **When**: invoked by `Verifier`'s loop on each high-severity issue.
- **What**: one issue → one scope-bounded `GraphPatch`.
- **Discipline**: must not touch nodes outside `issue.scope ∪
  neighborhood ∪ patch.add_nodes`. Validation rejects overreach.
- **Three paths** (dispatched by `GraphError` variant): L0Structural →
  read L2 + propose L0 patch; L1Semantic → call `L1Enricher` to rewrite
  L1; ScopeGap → propose new nodes/edges to fill the missing region.

### `PostExecutionValidator` (between Task and Review)

- **When**: optional, fires after dispatcher returns and before Review.
- **What**: deterministic check (e.g., `cargo check`, `pytest`) that runs
  the artifact and parses the output.
- **Three verdicts**: `Passed` → Review; `FailedAsGraphIssue` → bubble
  `GraphInvalid { source: PostExecutionValidation }` and SKIP Review;
  `FailedAsTaskIssue` → continue to Review (let LLM judge handle).
- **Why short-circuit Review on graph issues**: saves a model call when
  the cause is already proven by deterministic signal.

### `Reviewer` (Review phase)

- **When**: terminal acceptance gate.
- **What**: holistic verdict on whether the run satisfied the original
  task.
- **Layers**: deterministic backstops (graph consistency, sub-agent
  success, last_verification status) + LLM-as-judge that flags
  `RootCause::{GraphIssue, TaskIssue, ScopeIssue}`.
- **Routing**: pass → Done; fail with Graph/Scope → bubble GraphInvalid
  back to Graph phase; fail with Task → Done with embedded verdict
  (caller decides whether to retry Task phase).

### Why all four?

Each runs at a different point with different inputs and different
costs:

```
Verifier:          per Graph-phase round, sees graph + task + conv         (cheap-to-medium)
LocalRepairer:     per high-severity issue, sees scope + L2                (medium)
PostExecValidator: once per Task-phase completion, sees deterministic test (cheap, can short-circuit)
Reviewer:          once per Review phase, sees graph + outcomes + task     (expensive — LLM judge)
```

Collapsing any two would either lose cheap signals (the deterministic
validator's pattern-match) or run expensive signals (the Reviewer) more
often than needed. Each layer's cost matches its strategic role.

---

## 5. Sub-agent execution

### JSON-action protocol vs native tool_calls

DeepSeek (and OpenAI, Anthropic, etc.) all support native function-calling
where the model emits structured `tool_calls` in its response and the
runtime injects results via `role: "tool"` messages. Our sub-agent
doesn't use that protocol. Instead, it asks the model to emit a JSON
object with `{"action": ..., ...}` in plain message content. Three reasons:

1. **Portability.** The JSON-action protocol works with any model that
   follows instructions, including local Ollama runs without
   function-calling support, future backends, and degraded modes (e.g.,
   the model returns plain text → we treat it as a final answer).
2. **Three actions, not just two.** We added `report_graph_error` for
   sub-agents to bubble graph issues to the parent. Native `tool_calls`
   would mean inventing a fake tool for this signal; JSON-actions let
   it sit naturally alongside `use_tool` and `final_answer`.
3. **Inspectability.** Every assistant message is a single JSON string
   that's grep-able in transcripts. With native tool_calls, the protocol
   spreads across multiple structured fields.

The trade-off: we miss out on backend-side optimizations like parallel
tool calls and tool-result caching. For agent-as-orchestrator, the wins
in portability + transparency outweigh those.

### Why the loop is single-tier (no nested GraphLoop)

Each sub-agent is a single-shot tool loop, not a nested
GraphLoop-in-miniature. We considered nesting (each sub-agent runs its
own GRAPH ↔ TASK ↔ REVIEW for its local slice) but decided against it
for now:

- **Cost.** Nested loops would multiply token usage. With N sub-agents
  each running their own 5-round graph loop, we'd pay 5N graph rounds
  per Task phase.
- **Coordination.** Sub-agents would have parallel graphs that the
  parent has to merge, which re-introduces the "decisions become
  dispersed" failure mode Cognition warns about.
- **Diminishing returns.** Empirically (Demo A runs), single-shot
  sub-agents with rich context (full L0 + L1 + L2 at distance 0) and a
  tool loop are sufficient for most tasks.

Nested GraphLoops are a Phase 5+ option for when sub-agent tasks
themselves grow long enough to warrant their own discipline.

### Why `success=false` on graph error report

When a sub-agent emits `report_graph_error`, its `SubAgentResult.success`
is `false`. The discovery was valuable, but the sub-task wasn't
completed. The dispatcher's `all_succeeded` flag becomes false; the loop
bubbles `GraphInvalid` rather than going to Review.

We chose this over `success=true with graph_errors populated` because:

- The semantic is honest: this sub-task did not produce its assigned
  result; you cannot rely on its `output` field.
- `all_succeeded` becomes a single boolean the caller checks; downstream
  code doesn't need to inspect both success and graph_errors.

---

## 6. Repair architecture

### Why per-issue, never bulk

Three forces push toward bulk repair (do all the fixes at once, then
re-verify):

- Throughput: fewer model calls
- Atomicity: caller sees "the graph was fixed" not "9 patches landed"
- Simpler API: one `repair_all(errors) → patch` instead of
  `for err { repair(err) → apply }`

We chose per-issue anyway:

- **Bulk loses signal precision.** Each error is a specific contradiction
  between graph and reality. Bundling them into one prompt asks the
  model to weigh them simultaneously — which means it might fix only
  the salient ones and miss the rest.
- **Bulk creates risk of regression.** A model rewriting half the graph
  to fix three errors will sometimes introduce a fourth by accident.
  Per-issue patches are surgical: model touches only the scope you
  hand it.
- **Time-for-space is cheap with local models.** When you can run
  inference cheaply, paying 3 patches × N tokens beats 1 patch × 3N
  tokens that's also worse.

The time-for-space principle (#2 in the design principles) is the
generalization: prefer many small corrections over fewer bulk ones.

### Scope enforcement

`LocalRepairer::validate_scope` rejects a returned `GraphPatch` if it
touches nodes outside the issue's scope (or its 1-hop neighborhood, or
nodes the patch itself adds). The error message includes which node
overreached. This is mechanical, not heuristic: the model can be told to
stay in scope, but enforcement is the runtime's job.

### Why caller drives auto-repair, not GraphLoop

`GraphLoop` surfaces `GraphInvalid` to the caller; the caller calls
`LocalRepairer::repair_from_error` for each error, applies patches,
calls `resume_with_repaired_graph(repaired)`. Demo A does this in 30
lines.

We considered making auto-repair internal to GraphLoop (an
`auto_repair_budget` field on the config). Three reasons against:

- **Caller policy varies.** A CLI wants to print progress; a CI job
  wants to fail fast; a Web UI wants to ask the user. Internal
  auto-repair forces one policy.
- **Observability.** External auto-repair shows up in caller logs at
  the right level of abstraction; internal would be buried in
  GraphLoop's tracing.
- **Test surface.** A passive `GraphLoop` is easier to test (set up
  initial state → step → assert returned state) than one with
  configurable internal repair budgets.

The discipline is: the loop's API is "what just happened and what do
you want to do?" The caller decides whether retry is automatic.

---

## 7. Verification layering

### Three layers in `Verifier`

```
Layer 1 — structural (Graph::find_inconsistencies):
    Dangling edges, orphan nodes, cycles in acyclic-required relations,
    duplicate edges, invalid confidences.
    Deterministic. No model call. Always runs.

Layer 2 — model self-check:
    Given (graph, task, conv), is the graph sufficient for the task?
    What's missing / overstated / wrong?
    One model call. Skippable (`Verifier::structural_only()`).

Layer 3 — L1 sampling:
    Sample N nodes with non-blank L1. For each, fetch L2 via SourceLoader,
    ask model: does L1 still match L2?
    N model calls. Requires a configured loader.
```

The layers are gated on what's available, not on confidence: structural
checks always run; L2-comparing checks need a loader; model self-check
needs a model. A `Verifier::structural_only()` for unit tests bypasses
the model entirely.

### Why three, not one (just "ask the model")?

Letting the model judge everything in one prompt — "is this graph good
enough?" — has two failure modes:

- The model says "looks fine" even when the graph has a dangling edge
  (it doesn't bother to check structurally).
- The model says "fail" with vague reasoning, leaving the runtime no
  precise issue to repair.

Structured layers with typed issues solve both. Each `VerifyIssue` has
a `scope` (which nodes), a `severity`, a `source` (which layer found it).
`LocalRepairer` uses these to scope its repair patch.

---

## 8. Concurrency & cancellation

### Dispatcher uses `tokio::sync::Semaphore`

Inside `SubAgentPool::run_batch`, every sub-task is spawned as a tokio
future; a `Semaphore` caps the in-flight count to `max_concurrent`. This
gives us:

- True parallel execution (verified by `pool_actually_runs_batch_concurrently`)
- Bounded concurrency (verified by `pool_respects_max_concurrent_limit`)
- Order preservation (results returned in batch order, regardless of
  spawn-completion order, by collecting `JoinHandle`s in order then
  awaiting in order)

### Why no inter-batch cancellation

If sub-agent A in batch 1 reports a `report_graph_error`, batch 2 hasn't
started yet — we naturally don't run it (the loop returns
`GraphInvalid` before issuing the next batch). But within batch 1, the
other in-flight sub-agents are NOT cancelled; we wait for them and
collect their results too.

We considered "first error cancels the batch" but rejected:

- Cancellation in tokio requires either a `CancellationToken` plumbed
  through every future, or aborting `JoinHandle`s (which doesn't
  cleanly kill the underlying model call's HTTP request).
- Letting siblings finish often produces a *more useful* outcome — a
  graph error reported by sub-agent A plus a task result from B is
  more informative than A's error alone.
- For Phase 4 v1 the cost of running an already-spawned sub-agent is
  bounded by `max_steps` × per-call latency, so the "wasted" work is
  small.

### Join errors vs sub-agent errors

`SubAgent::execute` catches model errors internally and returns
`SubAgentResult::failure(...)`. The dispatcher only treats a tokio
`JoinError` (panic in the spawned future) as a fatal `HarnessError`.
This distinction matters: a sub-agent that times out, gets policy-denied,
or hits a network blip should not poison the batch — its result is
captured and the loop continues. Only a programming bug should kill the
batch.

---

## 9. Conversation & context management

### Why the graph snapshot is re-injected every model call

In `Conversation::to_request`, after the system prompt, we always inject
a second system message:

```
Current relationship-graph snapshot (authoritative — your beliefs about
the graph should match this):
{render of current graph}
```

This costs prompt tokens every turn, but eliminates the "stale graph in
the model's head" failure mode. The model never has to remember
intermediate graph states; it sees the current state every turn.

The alternative — only sending the graph delta — assumed the model could
accurately track the running state, which is the kind of implicit state
tracking that fails subtly in long conversations.

### Why we render the snapshot in plain text, not JSON

The graph snapshot in the prompt looks like:

```
graph version=3 status=Draft nodes=5 edges=4 l1_entries=5
nodes (L0 + L1 oneline):
  - id=auth.rs kind=File summary="handles JWT" L1="signs and verifies tokens" (c=0.85)
  - id=db.rs kind=File summary="storage" L1=(not yet enriched)
  ...
edges:
  [0] auth.rs -[Imports c=0.90]-> db.rs  evidence="use crate::db"
```

Plain text, not JSON. Reason: models reason better about a few well-laid
lines than nested JSON, and tokens are cheaper too. JSON-shaped fields
trigger pattern-matching ("this is data, just pass it through") whereas
prose-shaped state prompts richer reasoning.

### `Conversation` doesn't own the graph

`Conversation` carries the message history. The graph lives on
`GraphLoop`. Each `to_request` call takes the current graph snapshot as
a string parameter — meaning the same Conversation can carry through
multiple graph mutations without going stale. The boundary keeps each
type's responsibility narrow.

---

## 10. Tool layer

### `Tool` trait + `Policy` separation

```
Tool trait:    What can this tool do? What's its schema? How does its input classify?
Policy trait:  In this context, is this specific input allowed to run?
ToolContext:   Where? (cwd) How much output is too much? Which policy gates this call?
ToolRegistry:  Name → tool. invoke() is the single execution entry point.
```

A `Tool` declares per-input classification (`is_read_only`,
`is_destructive`, `is_concurrency_safe`). A `Policy` consults those
classifications + the tool name + the input to decide `Allow / Deny /
AskUser`. Every invocation routes through `ToolRegistry::invoke` → policy
check → `Tool::call` — the policy gate is the single chokepoint.

The default `SubAgent` policy is `DangerousCommandDeny`, not `AllowAll`:
it permits every registered tool freely, but blocks any bash command
whose `command` field matches a high-risk pattern (rm -rf /, mkfs,
`git push --force`, pipe-to-shell, etc.). A complementary
[`ScopeGuard`](src/tools/scope_guard.rs) is auto-derived per sub-task
from the task's `involved_nodes` and constrains bash writes to paths
under those nodes (or their distance-1 neighbors). Reads are
unconstrained by default — exploring the world is a legitimate model
behavior. The model is free to pick any registered tool; the two guards
are the only thing standing between it and the shell.

Why not put the gate on `Tool::call` itself? Because then every tool
would have to implement policy logic, and policy would be coupled to tool
implementation. With separation:

- New tools get policy gating for free.
- Custom policies (allowlist, time-of-day, role-based, audit-only)
  plug in at the `ToolContext` layer.
- `DangerousCommandDeny` (default) / `ReadOnly` / `AllowList` /
  custom `Policy` impls cover the common cases.

### Why per-input classification, not per-tool

A `BashTool` configured with `is_read_only` constant would be a lie:
`bash` itself isn't read-only; what determines it is the command. So
`BashTool::is_read_only(&self, input)` looks at the actual command:

- First-token allowlist (`ls`, `cat`, `grep`, ...)
- Multi-word prefixes (`git log`, `cargo check`, `rustc --version`, ...)
- Disqualifies anything with redirects, pipes, `$()`, `;`, `&&`

This is borrowed conceptually from Claude Code's `isReadOnly(input)`
pattern. The runtime trusts the per-call classification because the
discriminating information is in the input, not the tool's identity.

### Tail truncation, not head truncation

`truncate_tail(text, max_chars)` keeps the last `max_chars` chars and
prepends `[…N chars truncated…]` to mark the omission. Reason: command
output convention places the interesting bits at the end (error
messages, exit status, summaries). Head-truncation would routinely hide
the reason for failure. This is Claude Code's `EndTruncatingAccumulator`
pattern, transplanted.

---

## 11. Configuration & tier system

### Why two model tiers

A single agent run touches ~15 model calls. With one tier, you either
pay deep-model cost on everything (expensive) or accept fast-model
quality everywhere (graph decomposition gets sloppy). Two tiers let
the harness route calls to the right model:

| Component | Tier | Rationale |
|---|---|---|
| `GraphProposer` | fast | Many calls per run; each is short; quality is "good enough JSON" |
| `Verifier` | fast | Frequent re-checks; deterministic layer carries most of the load |
| `SubAgent` | fast | One per task; each is a single tool loop; pattern-matching dominates |
| `L1Enricher` | deep | One per node; output is structured semantic content that downstream depends on |
| `LocalRepairer` | deep | Patches must land on first try; cost of a bad patch > cost of a deeper call |
| `Decomposer` | deep | Task decomposition is high-leverage; one bad split costs N wasted sub-agents |
| `Reviewer` | deep | Single judgment call per run; quality matters more than throughput |

With `MODEL_NAME_FAST=deepseek-v4-flash MODEL_NAME_DEEP=deepseek-v4-pro`,
typical Demo A runs cost ~$0.03 instead of ~$0.10. The tier split also
cuts wall time: flash responds in 1-2 s vs pro's 5-15 s for the same
input.

### Why env-driven, not config-file-driven

`.env` is shell-native: every dev machine, every CI runner, every
container, every IDE understands env vars. A custom config format would
need a loader, schema, migrations, and a story for "where does it live."
`dotenvy` reads `.env` once at process start and falls back to existing
env — zero ceremony.

For programmatic callers, `ModelConfig::new(...)` lets you skip env
entirely and pass values directly.

---

## 12. Design principles, expanded

Six principles drive every component. The shortlist is in `README.md`;
here's the full reasoning.

### 1. Model-agnostic

Never hardcode a model name in source. Any naming flows through
`ModelConfig` reading env. Why: empirical work shows harness gains
transfer across models. Coupling the harness to a specific model wastes
that transfer.

In code, this means: anywhere you'd write `"gpt-4o"` or `"claude-opus"`,
you write `cfg.fast_model()` or `cfg.deep_model()` instead. Test code
that needs a specific model behavior uses a `MockModel` trait impl.

### 2. Time-for-space (拿错误换正确)

Prefer many small precise corrections over fewer bulk corrections. Each
error caught during execution is a precision signal — don't batch them.

In code: `LocalRepairer::repair_from_error` takes ONE error and returns
ONE patch. The Verifier re-runs after each patch. No `repair_all`
method.

This principle came from the user pushing back on a "batch errors then
fix together" suggestion early in design. The argument was: batching
loses the signal that each error encodes a specific contradiction.

### 3. Local graph repair, never bulk

When the verifier finds issues, fix them one at a time with a
subgraph-scoped patch. Global rebuilds are an explicit opt-in, not an
error path.

In code: `LocalRepairer::validate_scope` rejects patches that touch
nodes outside the issue's scope. There's no "rebuild the graph from
scratch" API — that would be a different operation handled at a
different layer.

### 4. Universality lives in the model, structure lives in the graph

The harness is generic across domains; domain-specific judgment is
delegated to the model. Don't put domain enums into shared types.

In code: `NodeKind` has `File / Function / Class / Module / Config /
Task / Other(String)`. The named variants are universal abstractions;
domain-specific kinds go in `Other("database")` and `Node.metadata`.
Same for `RelationType`.

Helps even when implementing a new domain — you don't have to
modify shared types to introduce, say, `NodeKind::TerraformResource`.
You stuff it in `Other("terraform_resource")` plus metadata; the
harness handles it generically.

### 5. Reviewer needs deterministic backstops

LLM-as-judge is unreliable alone. Layer multiple deterministic checks
before trusting the model's verdict.

In code: `Reviewer::review` runs deterministic checks (graph
consistency, sub-agent success, last_verification) and only then calls
the LLM judge. The verdict is `passed = det_passed && judge_passed`
(both must pass), and deterministic fail overrides judge pass (verified
by test `deterministic_fail_overrides_judge_pass`).

### 6. Scanners are seeds, not the product

Code/data/infra scanners produce low-confidence starter graphs (≤ 0.6).
The model is the real graph builder. Don't over-invest in scanner
cleverness.

In code: `CodeScanner` emits edges with `confidence: 0.6` (literal
constant). Phase 2.5's L1Enricher is the model-driven path that
produces high-confidence semantic content. The scanner exists so the
model has *something* to start from in code-domain runs; for non-code
domains there is no scanner at all (`NullSourceLoader`).

---

## 13. Trade-offs and known limitations

### Token cost vs accuracy

Every design choice that increases accuracy (graph snapshot every turn,
re-verify after every patch, deterministic Reviewer backstops, multiple
tiers) costs tokens. Typical Demo A run uses 40-70K tokens. We've
prioritised accuracy because:

- Tokens are cheap and getting cheaper (deepseek-v4-flash is < $0.30
  per million)
- Wrong work is expensive — caught wrong work compounds; uncaught wrong
  work goes to production
- The harness is designed for non-trivial tasks where the alternative
  to spending tokens is human review time

If your task is small enough that tokens dominate, configure
`Verifier::structural_only()` (skip model self-check), `Reviewer::deterministic_only()`
(skip LLM judge), and skip the validator. The harness still works; it
just leans more on the cheap layers.

### `max_tokens` and reasoning models

Reasoning models (DeepSeek-v4-pro, GPT-o1, Claude with extended
thinking) burn 5-20K tokens of internal reasoning before emitting the
visible JSON action. Output capped at 8K → JSON truncated → "unterminated
JSON object" error. We bumped `GraphProposer.max_tokens` to 32K as the
default, but the underlying issue is that reasoning models change the
relationship between max_tokens and useful output.

Mitigation in the harness:
- Bumped `max_tokens` defaults to 32K (Proposer) / 8K (Decomposer) etc.
- Tier split routes the high-volume calls to `flash` (non-reasoning)
  where this isn't a problem
- The JSON parser tolerates plain-text responses (treats them as final
  answer in SubAgent; surfaces as `ProposerStep` parse error elsewhere)

### No nested GraphLoop in sub-agents (yet)

Sub-agents are single-shot tool-calling loops. They can read source via
`bash`, but they can't run their own discovery → repair → execute cycle
on a private subgraph. For tasks where sub-tasks are themselves
non-trivial (e.g. "design and implement a new module"), this shows up as
sub-agent failure-to-converge.

Phase 5+ scope item: nested GraphLoop with shared L0+L1 from parent and
private L2 access.

### No streaming output

The `OpenAICompatModel` makes a single non-streaming HTTP call per
`complete()`. For long deep-model calls (20+ seconds), the caller sees
no progress indicator. Streaming would require:
- Server-sent events parser in the HTTP client
- `ModelResponse::stream` variant or a separate API
- UI/CLI integration for incremental display

Streaming would also subtly change the `max_tokens` story (truncation
happens at the end of stream rather than as a single rejected response).

### No persistence

`Graph::to_json` serializes to JSON. There's no built-in session store,
no checkpoint format, no resume-across-process. A CLI binary that
crashes mid-run loses everything. For long-running production agents
this is required infrastructure but not in Phase 4 scope.

### No formal tool-call protocol fallback

If a user really wants OpenAI native `tool_calls` (for parallel tool
execution, server-side caching, etc.), they'd need to:
- Extend `Message` with `tool_call_id` and `tool_calls` fields
- Update `OpenAICompatModel` serialization
- Rewrite `SubAgent::execute` to use the native protocol
- Add a tool definition serializer

It's straightforward but non-trivial. JSON-action protocol covers the
ground we need today.

---

## 14. What we considered and rejected

### Reactive vs proactive verification

Considered: only run the Verifier when something fails (cheaper). Picked
proactive: run it every time the proposer says `ReadyForVerify`. Reason:
cheaper to catch issues early than to backtrack from a failure that
happened three phases later.

### Single Reviewer / Verifier / Validator unified class

Considered: one `Acceptor` trait with three implementations. Picked
three separate classes. Reason: each has a different signature
(`Verifier` takes `Option<&Conversation>`, `Reviewer` takes a
`DispatchOutcome`, `PostExecutionValidator` returns a verdict enum
rather than `VerificationResult`). Unifying them would force the
caller to construct dummy values for fields that don't apply.

### Graph stored as RDF triples or SQLite

Considered: store the graph in a real DB for query power. Picked
in-memory `HashMap<NodeId, Node>` + `Vec<Edge>`. Reason: the graph is
the working memory of a single run, not durable state. The cost of an
on-disk format (serialization, query layer, schema migrations) far
exceeds the benefit for our access patterns (BFS, local subgraph, full
scan).

### Auto-routing model tiers based on call complexity

Considered: have the harness auto-pick fast vs deep tier based on the
prompt size or task complexity. Picked explicit per-component tier
assignment. Reason: tier assignment is a deliberate policy decision
(quality-vs-throughput trade-off), not a function of input. Auto-routing
would hide the policy and make cost unpredictable.

### Generic `Result<T, GraphError>` for everything

Considered: replace `HarnessError` with a domain-specific error type
that distinguishes graph errors from network errors from JSON errors
from policy errors. Picked a flat `HarnessError` enum with String
payloads. Reason: the call sites that care about specific error types
already use pattern matching on the variant (model / domain / scanner /
io / serde / context / scheduler / graph). Beyond that, error context
is best conveyed in the string. Adding more structure cost more than
it returned.

---

## 15. References

The harness draws on a few public sources of intellectual capital:

- **Anthropic, "Building Effective Agents"** (anthropic.com/research/building-effective-agents)
  — orchestrator-workers and evaluator-optimizer patterns, the "agent
  vs. workflow" framing, the discipline of stopping conditions.
- **SWE-agent paper** (arXiv:2405.15793) — empirical evidence that
  Agent-Computer Interface design is a first-order performance lever.
- **Reflexion paper** (arXiv:2303.11366) — verbal reinforcement /
  episodic memory. Our `GraphError` → repair flow is a materialised
  variant where reflections become graph edits.
- **LangGraph** (langchain-ai/langgraph) — graph-as-control-flow; we
  use graph-as-world-model. Different layers, but the precedent for
  graph-shaped agent state is worth citing.
- **Cognition, "Don't Build Multi-Agents"** (cognition.ai/blog/dont-build-multi-agents)
  — the warning that parallel sub-agents tend to drift apart. Our
  defense is the shared graph as the substrate. Whether this is
  sufficient defense is a Phase 5+ question.
- **Claude Code source** (reconstructed from the public npm package's
  sourcemap) — read as reference for tool architecture, per-input
  classification, tail truncation, and the `isReadOnly` pattern. No
  code is copied verbatim; the patterns are reimplemented in Rust.

For the deeper "binding constraint" framing (harness > model on long
tasks), the underlying argument shows up across the above sources
without being attributed to one paper. Our project bets on that
framing being correct.
