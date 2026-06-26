# Graph-Centric Agent Harness

A Rust implementation of a universal LLM agent orchestrator built around one
thesis: **every agent task is fundamentally an operation on a relationship
graph.** Domain knowledge — code, infrastructure, research, planning — enters
through a single seam; the orchestrator stays generic.

## Core idea

The graph is **not** a passive data store or a transcript of what happened.
It is the **orchestrator's plan**: the LLM (acting as main agent) maintains
the graph as its working memory, and the loop is:

1. **Plan.** Main agent reads the graph and either (a) extends it with new
   sub-nodes for a clearly-scoped task (Mode A — clear plan) or (b) emits
   `ask_user` to clarify scope with the user before drawing nodes
   (Mode B — exploratory).
2. **Dispatch.** Sub-agents execute one sub-node each, using the graph as
   context. They do **not** edit the graph — only report success/failure
   with evidence.
3. **Review.** Per-node review against the orchestrator's spec. Pass → node
   is marked done. Fail → orchestrator writes a **local** `GraphPatch`
   (one sub-node's spec, not the whole graph) and the loop re-dispatches
   that one sub-agent.

This is what "graph-centric" means in code: every state mutation is a
`GraphPatch` of a specific scope. The LocalRepairer used for verifier
findings and the per-sub-agent-failure re-dispatch are the same mechanism —
just different triggers.

```
                  task description
                         |
                         v
        +---------- GRAPH ---------+ <----+
        |  propose -> verify ->   |      | repair
        |   repair (local) ->     |      |
        |   L1 enrich              |      |
        +-----------+--------------+      |
                    | verifier pass       |
                    v                     |
        +---------- TASK ----------+      |
        |  decompose -> dispatch  |      |
        |   (parallel sub-agents  |      |
        |    with tool loop)       |      |
        +-----------+--------------+      |
                    | all succeeded       |
                    v                     |
        +-- POST-EXECUTION VALIDATE -+    |
        |  (cargo check / pytest /  |    |
        |   custom; pattern-match   |    |
        |   stderr for graph hints) |    |
        +-----------+----------+----+    |
            graph    |   task         |
            issue    | issue / pass   |
                |    v                |
                |  +---------- REVIEW ----------+
                |  |  determ. checks +          |
                |  |   LLM-as-judge             |
                |  +--+----------------+--------+
                |     | pass           | fail (graph / scope)
                v     v                |
            GraphInvalid              Done                |
            (4 sources: <--------------------------------+
             VerifierStalemate,
             DuringExecution,        ^ surfaces to caller;
             PostExecutionValidation,  caller auto-repairs
             Review)                   and resumes
```

## What this is

- **Generic LLM agent orchestrator** in pure Rust. ~15K LOC, single binary,
  zero runtime dependencies, 606 unit/integration tests.
- **Model-agnostic.** Speaks OpenAI-compatible HTTP — works with DeepSeek,
  vLLM, Ollama, OpenAI, OpenRouter, Anthropic-via-proxy, or anything that
  serves `/v1/chat/completions`. Reasoning-only models (DeepSeek-v3,
  MiniMax M3) are first-class: every layer has a `text_or_reasoning()`
  fallback that reads from `reasoning_content` when `content` is empty.
- **Three-layer relationship graph** (L0 structure / L1 semantics / L2 data)
  as the shared substrate between the main agent, sub-agents, and the user.
- **State machine with explicit phase transitions**, not a free-form ReAct
  loop. Every transition is one method call returning a typed `LoopState`.

## Quick start

### 1. Configure a backend

Copy `.env.example` to `.env`, fill in:

```bash
MODEL_BASE_URL=https://api.deepseek.com/v1   # OpenAI-compatible endpoint
MODEL_API_KEY=sk-...                          # bearer token (omit for local)
MODEL_NAME_FAST=deepseek-v4-flash             # high-volume calls (Proposer, SubAgent)
MODEL_NAME_DEEP=deepseek-v4-pro               # quality-sensitive calls (Enricher, Repairer, Decomposer, Reviewer)
```

Or set `MODEL_NAME_DEFAULT` to route both tiers to the same model.

### 2. Verify connectivity

```bash
cargo run --bin probe_model
```

Sends a single "ping" to each tier and reports latency + token usage. Use
this before any longer run to catch URL / key / model-name mismatches.

### 3. Run the main demo

```bash
cargo run --bin agent_a -- "your task here"
```

Or omit the argument to be prompted. The agent:

1. **Intake.** If the task is vague, the main agent emits `ask_user` first
   to clarify scope before drawing any graph nodes. If the task is clear,
   it proceeds directly to planning (Mode A vs Mode B in the Core Idea
   section above).
2. Builds a relationship graph through conversation (asks you clarifying
   questions when needed).
3. Decomposes the task into sub-tasks based on the graph.
4. Dispatches sub-agents concurrently (each with `bash` tool access under a
   read-only policy).
5. Runs `cargo check` as a post-execution validator (configurable).
6. Reviews the result with deterministic checks + LLM-as-judge.

Outputs land in `./demo_output/`:

- `agent_a_graph.json` — final L0 + L1 graph
- `agent_a_transcript.txt` — full conversation
- `agent_a_task_outcome.json` — sub-agent results
- `agent_a_review.json` — review verdict

## The three-layer graph

Per the v2.0 design, the graph is layered so structure, semantics, and raw
content can each be refined independently:

| Layer | Name | Content | Source | Mutable? |
|---|---|---|---|---|
| **L0** | skeleton | nodes + edges | scanner + model | yes (patches) |
| **L1** | muscle | per-node `{responsibility, implementation, design_intent, constraints}` + confidence | model reads L2, writes L1 | yes (re-enrich) |
| **L2** | skin | raw bytes (source files, configs, schemas, ...) | direct read on demand | never stored in graph |

L0 patches trigger L1 re-enrichment for the new nodes. L1 drift relative to
L2 triggers re-enrichment of the drifted node. L2 changes (e.g., sub-agent
edits a file) eventually trigger L0 + L1 updates.

## Architecture philosophy

The Core Idea section above is the **what** — plan, dispatch, review. This
section is the **why**: the design choices that shape every component, and
the trade-offs they accept. If you're contributing, these are the decisions
to preserve.

### The graph is the plan, the schedule, and the audit log

Three different jobs, all held by the same data structure:

- **Plan** — the main agent edits the graph as its working memory. There is
  no separate "scratchpad" or hidden state; every intent the model has is
  a `GraphPatch`.
- **Schedule** — `DagScheduler` runs Kahn's algorithm over the `DependsOn`
  edges and produces **wave-aligned batches**. Two independent tasks
  automatically land in the same wave; a dependent task waits for its
  prerequisites' wave to complete. The graph's structure determines the
  concurrency — the dispatcher doesn't invent scheduling, it executes
  what the graph already encodes.
- **Audit log** — `CheckpointStore` snapshots `(round, phase, graph, transcript)`
  after every meaningful change, with a `branches` map for forks. You can
  rewind, replay, or branch an exploratory variant from any historical
  state. Combined with `Graph::version` (bumped on every patch), every
  step is traceable.

### Determinism before LLM judgment (defense in depth)

The system has many "trust the model" decisions. None of them is the
**hard gate**. Every one is checked at least twice — once by a
deterministic mechanism, once by an LLM-as-judge advisor:

| "Trust the model" decision | Deterministic second line | LLM advisor |
|---|---|---|
| "Graph is structurally consistent" | `Graph::find_inconsistencies` (dangling edges, cycles, duplicates) | (none — too simple for LLM) |
| "Sub-agent's work is right" | `CheckContract` (`KnowHow` mention, `Exploratory` cap, `MustEdit` write-call count) — checked **twice**: by the sub-agent and re-checked by the dispatcher | (none) |
| "Code compiles" | `PostExecutionValidator` runs `cargo check`/`tsc` and pattern-matches stderr for graph vs task errors | (none) |
| "L1 matches L2" | substring comparison + drift severity | `l1_check_verdict` (advisory; never unilaterally fails) |
| "Sub-agent's claim of done is honest" | dispatcher re-evaluates the contract after the sub-agent returns | (none) |
| "Final result is acceptable" | deterministic reviewer (graph consistency, sub-agent success, verify-phase status) | `judge_verdict` (advisory; root_cause routes to repair) |

**A flaky model cannot take down a structurally sound graph.** This is the
single most important safety property of the system. Any new "trust the
model" decision must come with a deterministic second-line check, or it
doesn't ship.

### Narrow protocols at boundaries, rich protocols inside

A pattern that recurs across the codebase: **the deeper into the system,
the narrower the protocol.** This is a deliberate design choice, not an
oversight.

| Layer | Protocol | Width | Why narrow |
|---|---|---|---|
| Main agent | OpenAI `tool_calls` (6 step types) | Rich | Orchestration needs flexibility |
| Sub-agent | Custom JSON-action (`use_tool` / `final_answer` / `report_graph_error`) | Narrow | Constrained exec env (`max_steps=8`, no direct graph access); narrow = easy to verify |
| Skill compile | `NodeKind::Task` + `DependsOn` only | Narrower | Skills are cached, replayed, trusted; narrow = safe cache |

The first instinct on encountering this is to **unify** — make the sub-agent
also use `tool_calls`, make skills emit full `GraphPatch`es. Don't. Each
narrowing is a defense-in-depth decision: the narrower the contract at a
boundary, the smaller the blast radius if a model misbehaves at that layer.
If a future contributor proposes unifying protocols across boundaries, the
question to ask is: **what safety guarantee do we lose?**

### Three orthogonal memory tiers

The system has three distinct, complementary "memories":

| Tier | Storage | Lifetime | What's in it |
|---|---|---|---|
| **Structural** (graph) | `Graph` in memory + checkpointed | The run | L0 nodes/edges + L1 descriptions — the orchestrator's plan |
| **Prompt** (conversation) | `Conversation` in memory | The run | LLM chat history — what the model has seen, including `ask_user` exchanges, verifier rejections, repair attempts |
| **Compiled** (skills) | `LocalSkillStorage` (filesystem) | Permanent | Extracted task DAGs that worked, indexed by Jaccard-token similarity |

These are **orthogonal**: skills don't leak into the graph, the graph doesn't
leak into the conversation, the conversation doesn't leak into skills. New
"memory" features should pick a tier and a write path; resist the urge to
put it everywhere.

### Skills are structural memory, not prompt memory

When a run successfully reaches `ready_for_verify`, the orchestrator extracts
the `propose_patch` sequence as a compiled task DAG and stores it locally
(`LocalSkillStorage`). The next run with a token-Jaccard ≥ 0.25 task
**skips the decomposer entirely** and uses the compiled skill graph directly.
This is structural memory: the skill is a graph topology, not a prompt
snippet. Successful runs compound — the agent gets faster at things it's
already done, and the speedup is grounded in the same kind of artifact
that drives everything else (a `Graph`).

### Sub-agents are constrained, not trusted

Sub-agents run with three independent constraints, all enforced in code:

1. **`max_steps`** (default 8) — hard cap on the number of model calls per sub-agent.
2. **`ScopeGuard`** — every `use_tool` action is checked against an allowed-path
   policy **before** invocation. A sub-agent dispatched to "fix `auth.rs`"
   cannot write to `/var/log` or `~/.ssh/`. The bounded context is enforced
   at the **filesystem level**, not just the cognitive level.
3. **`CheckContract`** — the sub-agent's `final_answer` is validated against
   a deterministic predicate (must mention expected phrases, must stay
   within a region, must have made write tool calls for "must-edit" tasks).
   The check runs **twice** — by the sub-agent itself, and re-checked by
   the dispatcher. Either layer can fail the run.

Plus a `report_graph_error` action that lets a sub-agent **bubble** a
`GraphError` up to the main loop when it discovers the graph is wrong —
this is the sub-agent's voice in the repair process.

### Two intake modes, code-gated

Round 0 (the first round of a fresh conversation) has a gate. Vague
tasks (heuristic: vague starter phrases EN+ZH, short with no verb, single
word) must emit `ask_user` before drawing any graph nodes. Clear tasks
may emit `propose_patch` directly. The gate is the second line of defense
— the system prompt also teaches Mode A vs Mode B, but prompt-only is
not load-bearing. **Errs on the side of letting through**: a false
positive is an annoying `ask_user`; a false negative wastes the run on a
graph built from a vague intent.

### The graph is the public API

Despite `graph_loop.rs` being ~6.7K LOC, the entire public API of the run
loop is **5 variants** in `LoopState`: `Running` (continue stepping),
`Paused` (waiting for `ask_user` answer), `GraphInvalid` (caller must
repair), `Done` (terminal success), `Error` (terminal failure). The web
gateway only ever sees these 5; everything inside the loop is private.
This is the discipline that lets the core refactor freely without
breaking the gateway.

## State machine

`GraphLoop::step()` advances one beat and returns a `LoopState`:

```
pub enum LoopState {
    Running,                                          // continue stepping
    Paused { question, rationale },                   // ask user, then resume(answer)
    GraphInvalid { source, errors, snapshot },        // caller repairs, resume_with_repaired_graph(g)
    TaskFailed { failures },                          // sub-agents failed at code level
    Done(FinalResult),                                // terminal: pass
    Error(String),                                    // terminal: poisoned
}
```

`GraphInvalid` is the central recovery state. It can be raised from four
sources, and the caller handles all of them through the same `resume_*`
methods:

| `ErrorSource` | Origin | Trigger |
|---|---|---|
| `VerifierStalemate` | inside Graph phase | LocalRepairer's repair budget exhausted |
| `DuringExecution` | inside Task phase | a sub-agent's JSON action was `report_graph_error` |
| `PostExecutionValidation` | between Task and Review | configured validator (e.g., `cargo check`) saw graph-error patterns in failure output |
| `Review` | inside Review phase | LLM judge returned `verdict: fail` with `root_cause: graph` or `scope` |

In all four cases, the caller iterates the errors, calls
`LocalRepairer::repair_from_error` on each, applies patches to the snapshot,
then calls `gl.resume_with_repaired_graph(repaired)`. Demo A wraps this in
an auto-repair loop capped at 3 cycles; production callers can wire human
review or escalation policies of their choice.

## Components

| Module | Role | Key Types |
|---|---|---|
| `graph::` | L0 storage + traversal + validation | `Graph`, `Node`, `Edge`, `NodeId`, `NodeKind`, `RelationType`, `GraphPatch`, `Inconsistency`, `L1Description`, `L1Store` |
| `scheduler::` | Topological batch scheduling (Kahn-based waves) | `DagScheduler`, `Schedule` |
| `context::` | Sub-agent context assembly | `ContextBuilder`, `ContextBudget`, `SourceLoader`, `FilesystemSources`, `InMemorySources`, `NullSourceLoader` |
| `model::` | Model abstraction + OpenAI-compat client | `Model` trait, `OpenAICompatModel`, `ModelConfig`, `Message`, `ModelRequest`, `ModelResponse`, `StreamDelta` |
| `model::text_or_reasoning()` | Reasoning-content fallback (DeepSeek/M3) | (method on `ModelResponse`) |
| `tools::` | Tool surface + Bash execution | `Tool` trait, `ToolRegistry`, `ToolDef`, `ToolOutput`, `ToolContext`, `Policy` (`AllowAll`/`ReadOnly`/`AllowList`), `BashTool`, `ScopeGuard`, `truncate_tail` |
| `agent::conversation` | Multi-turn dialog state | `Conversation` |
| `agent::intake` | Mode A/B intake gate (vague → ask_user) | `classify_task_clarity`, `check_intake_compliance` |
| `agent::proposer` | Main-agent step engine (6 step types) | `GraphProposer`, `ProposerStep` (`AskUser` / `Explore` / `ProposePatch` / `ReadyForVerify` / `Block` / `ConsultAdvisor`) |
| `agent::verifier` | Three-layer verification (structural + model self-check + L1 sampling) | `Verifier`, `VerifyIssue`, `VerificationResult`, `Severity` |
| `agent::repairer` | Scope-bounded local repair (L0Structural / L1Semantic / ScopeGap) | `LocalRepairer` |
| `agent::enricher` | Model reads L2, writes L1 | `L1Enricher` |
| `agent::decomposer` | Model breaks task into sub-task DAG | `Decomposer` |
| `agent::subagent` | Sub-task executor (JSON-action protocol + ScopeGuard + contract self-check) | `SubAgent`, `SubTask`, `SubAgentResult` |
| `agent::dispatcher` | Wave-aligned concurrent batch execution | `Dispatcher`, `SubAgentPool`, `DispatchOutcome` |
| `agent::reviewer` | Final acceptance gate (deterministic + LLM judge) | `Reviewer`, `ReviewResult`, `JudgeVerdict`, `RootCause` |
| `agent::validator` | Post-execution validator (between Task and Review) | `PostExecutionValidator`, `ValidationVerdict`, `BashCheckValidator` |
| `agent::cascade` | Cascade backtracking on sub-agent failure | `CascadeBacktracker`, `PredecessorVerdict` |
| `agent::cascade_expand` | L0→L1→L2 expansion in Task phase | `expand_graph` |
| `agent::contract` | Sub-agent dispatch contracts (deterministic) | `CheckContract` (`KnowHow` / `Exploratory` / `MustEdit` / `None`) |
| `agent::graph_loop` | The state machine | `GraphLoop`, `LoopState`, `GraphError`, `L0ErrorType`, `ErrorSource`, `FinalResult` |
| `skills::` | Skill capture, match, compile, store | `matcher` (Jaccard), `capture`, `compiler` (pure Skill → task DAG), `retrieve`, `LocalSkillStorage` |
| `web::` | HTTP/WS gateway | `api_runs`, `ws`, `events`, `heartbeat`, `persistence`, `checkpoint` (CheckpointStore + branching), `state` |
| `domain::` | Domain-injection seam | `Domain`, `Scanner`, `ToolRegistry` (trait), `DomainValidator`, `TaskNeeds` |
| `domain::code::` | Example domain: code project scanner | `CodeScanner` |

## Configuration knobs

### `ModelConfig` (read from env)

| Var | Required | Purpose |
|---|---|---|
| `MODEL_BASE_URL` | yes | OpenAI-compatible endpoint, must end in `/v1` |
| `MODEL_API_KEY` | usually | Bearer token; leave blank for local backends |
| `MODEL_NAME_FAST` | yes (or set DEFAULT) | High-volume tier — Proposer, Verifier, SubAgent |
| `MODEL_NAME_DEEP` | yes (or set DEFAULT) | Quality tier — Enricher, Repairer, Decomposer, Reviewer |
| `MODEL_NAME_DEFAULT` | optional | Fallback for both tiers when explicit tier vars are unset |

### `GraphLoopConfig`

| Field | Default | Purpose |
|---|---|---|
| `max_rounds` | 24 | Outer cap on Graph-phase steps |
| `max_repair_rounds` | 3 | Inner cap on LocalRepairer attempts per Verifier failure |
| `tool_cwd` | `.` | Working directory for tool calls |
| `tool_output_cap` | 12_000 | Per-call output truncation threshold (tail-keep) |
| `tool_policy` | `AllowAll` | Default policy for tools invoked through the loop's registry |

### Sub-agent tool policy

`BashTool` classifies commands as read-only based on a whitelist:

- Read inspection: `ls`, `cat`, `head`, `tail`, `grep`, `find`, `pwd`, `wc`, ...
- Git observation: `git status`, `git log`, `git diff`, `git show`, ...
- Dev versions: `cargo --version`, `rustc --version`, `node --version`,
  `python --version`, `go version`, `java -version`, ...
- Dev inspection: `cargo check`, `cargo metadata`, `cargo tree`, `npm list`,
  `pip show`, `go env`, `kubectl get`, `docker ps`, ...
- Anything with pipes, redirects, `$()`, `;`, `&&` — disqualified

The `ReadOnly` policy permits only commands that classify as read-only.
`AllowAll` permits everything. Custom policies implement the `Policy` trait.

## Phase status

| Phase | Status | What's in it |
|---|---|---|
| 1 (substrate) | done | Graph types, traversal, validation, scheduler, context builder, code scanner |
| 2 (framework) | done | Model trait + OpenAI-compat, Conversation, Tool layer, Proposer, Verifier, LocalRepairer, GraphLoop |
| 2.5 (three-layer) | done | L1Description, L1Store, L1Enricher, Verifier L1 sampling, Repairer L0/L1/Scope split, ContextBuilder L1 rendering, GraphLoop L1 auto-enrich |
| 3 (task phase) | done | Decomposer, SubAgent + tool-calling loop, Dispatcher, SubAgentPool |
| 4 (review + bubble + auto-repair + validator) | done | Reviewer (det. + LLM judge), sub-agent `report_graph_error` action, Demo A auto-repair, PostExecutionValidator + `BashCheckValidator` |
| 5 (v2) | done | Cascade backtracking, WebSocket, Vue 3 frontend, checkpoint persistence, multi-profile config, skill-to-graph compiler, 3D graph panel, streaming output, git workflow, heartbeat self-improvement loop, Hook system, quality metrics scanner, fractal L0/L1/L2 architecture |

## What's runnable now

| Binary | Command | Purpose |
|---|---|---|
| `probe_model` | `cargo run --bin probe_model` | Backend connectivity smoke; prints latency + tokens per tier |
| `agent_a` | `cargo run --bin agent_a -- "<task>"` | Domain-agnostic main-agent demo (Phase 3 + 4 enabled) |
| `demo` | `cargo run --bin demo` | Phase 1 deterministic scanner demo (scans `./src` into a graph) |
| `graph_harness` | `cargo run --bin graph_harness` | Minimal Phase 1 smoke (constructs a tiny graph and prints it) |

## Tests

```bash
cargo test --lib                 # all 606 unit + integration tests
cargo test --lib agent::         # only agent layer
cargo test --lib tools::bash::   # only bash tool
cargo test --lib graph::         # only graph types
```

Live model tests are gated behind `LIVE_MODEL_TEST=1`:

```bash
LIVE_MODEL_TEST=1 cargo test --lib model::openai_compat
```

## Documentation

- **[English](README.md)** (this file) — quick start, what it does, honest scope
- **[简体中文](README.zh-CN.md)** — 中文版
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — design rationale, rejected alternatives, trade-offs
- **[ARCHITECTURE.zh-CN.md](ARCHITECTURE.zh-CN.md)** — 中文版架构文档
- **[Design v2: Cascade Backtracking](docs/design-v2-cascade-backtrack.md)** — next-generation design with immutable anchor, cascade-backtrack verification, and weak-model-first philosophy
- **[设计 v2: 级联回溯](docs/design-v2-cascade-backtrack.zh-CN.md)** — 中文版 v2 设计文档
- `docs/superpowers/specs/` — design specs (English) for the two completed tool-system features
- `docs/superpowers/plans/` — implementation plans (English) for the same

## Repository layout

```
src/
├── lib.rs                     # crate root + re-exports
├── main.rs                    # legacy Phase 1 smoke binary
├── error.rs                   # HarnessError + Result alias
├── graph/
│   ├── mod.rs                 # Node, Edge, Graph, GraphPatch, NodeKind, RelationType
│   ├── traversal.rs           # BFS, subgraph extraction, dependents_of, distance_to
│   ├── validation.rs          # Inconsistency detection (dangling, cycle, duplicate, ...)
│   └── l1.rs                  # L1Description + L1Store
├── scheduler/mod.rs           # DAG topological scheduler with batching
├── context/mod.rs             # ContextBuilder, three-layer rendering, distance-based compression
├── model/
│   ├── mod.rs                 # Model trait, Message, ModelRequest, ModelResponse,
│   │                          #   ModelResponse::text_or_reasoning() helper
│   ├── openai_compat.rs       # OpenAI-compatible HTTP client (reqwest) + SSE streaming
│   └── config.rs              # ModelConfig — env-driven tiered loading
├── tools/
│   ├── mod.rs                 # Tool trait, ToolRegistry, Policy, ToolContext, truncate_tail
│   ├── bash.rs                # BashTool with classify_read_only + ReadOnly-policy whitelist
│   ├── file.rs                # ReadFile / WriteFile / EditFile
│   └── policy.rs              # ScopeGuard (filesystem-level path policy)
├── agent/
│   ├── mod.rs                 # agent-layer re-exports
│   ├── conversation.rs        # Multi-turn dialog state
│   ├── intake.rs              # Mode A/B intake gate (vague → ask_user)
│   ├── proposer.rs            # Main-agent step engine (6 step kinds, OpenAI tool_calls)
│   ├── verifier.rs            # Three-layer verification (structural + L1 + graph self-check)
│   ├── repairer.rs            # Scope-bounded local patch generation (3 paths)
│   ├── enricher.rs            # Model reads L2 -> writes L1
│   ├── decomposer.rs          # Model -> task DAG (NodeKind::Task + DependsOn edges)
│   ├── subagent.rs            # JSON-action sub-task executor + ScopeGuard
│   ├── dispatcher.rs          # Wave-aligned batch execution (SubAgentPool + Dispatcher)
│   ├── reviewer.rs            # Deterministic + LLM-as-judge acceptance gate
│   ├── validator.rs           # PostExecutionValidator trait + BashCheckValidator
│   ├── cascade.rs             # Cascade backtracking on sub-agent failure
│   ├── cascade_expand.rs      # L0→L1→L2 cascade expansion in Task phase
│   ├── contract.rs            # CheckContract (KnowHow / Exploratory / MustEdit / None)
│   └── graph_loop.rs          # The state machine (Phase + GraphPhase enums)
├── skills/                    # Skill capture, match, compile, store
│   ├── matcher.rs             # Token-based skill scoring (Jaccard)
│   ├── compiler.rs            # Pure Skill → task DAG transformation
│   ├── capture.rs             # Auto-capture from successful runs
│   ├── retrieve.rs            # list_for_prompt + find_and_load_matching_skills
│   ├── prompt_registry.rs     # Dynamic prompt blocks (Claude Code style)
│   ├── storage.rs             # LocalSkillStorage (~/.local/share/...)
│   └── types.rs               # Skill, SkillMeta
├── web/                       # axum HTTP/WS server
│   ├── api_runs.rs            # /api/runs/* + drive_run (the main run driver)
│   ├── ws.rs                  # WebSocket handler (/ws/runs/:id)
│   ├── events.rs              # RunEvent enum (StreamChunk / StreamToolCall / StreamEnd / ...)
│   ├── heartbeat.rs           # Self-improving loop across process lifetimes
│   ├── persistence.rs         # Run persistence (data/runs/<id>/)
│   ├── checkpoint.rs          # CheckpointStore + branching (git-for-runs)
│   └── state.rs               # WebState, EngineConfig, LoopTuningConfig
├── domain/
│   ├── mod.rs                 # Domain, Scanner, ToolRegistry trait, DomainValidator, TaskNeeds
│   └── code/                  # Example domain: code-project scanner stub
└── bin/
    ├── probe_model.rs         # Backend connectivity smoke
    ├── demo.rs                # Phase 1 scanner demo
    └── agent_a.rs             # Main agent demo (Phase 3 + 4)
```

## Design principles

These shape every component. Most are explained in detail in the
**Architecture philosophy** section above; this list is the TL;DR.

1. **Model-agnostic.** Never hardcode a model name in source; all model
   selection flows through `ModelConfig` reading env. Reasoning-only
   models (DeepSeek-v3, MiniMax M3) are first-class — every layer
   routes through `ModelResponse::text_or_reasoning()`.
2. **The graph is the plan, the schedule, and the audit log.** Three
   jobs, one data structure. See *Architecture philosophy* for details.
3. **Determinism before LLM judgment.** Every "trust the model" decision
   has a deterministic second-line check. A flaky model cannot take down
   a structurally sound graph. See *Architecture philosophy*.
4. **Narrow protocols at boundaries, rich protocols inside.** Main
   agent uses OpenAI `tool_calls`; sub-agent uses custom JSON-action;
   skill compile uses Task + DependsOn. Each narrowing is a
   defense-in-depth decision. See *Architecture philosophy*.
5. **Three orthogonal memory tiers.** Structural (graph), prompt
   (conversation), compiled (skills) — never leak between them. See
   *Architecture philosophy*.
6. **Skills are structural memory, not prompt memory.** Successful runs
   capture their `propose_patch` sequence as compiled task DAGs and store
   them for reuse at Jaccard ≥ 0.25. See *Architecture philosophy*.
7. **Sub-agents are constrained, not trusted.** `max_steps` + `ScopeGuard`
   + `CheckContract` (double-checked) + `report_graph_error` bubble. See
   *Architecture philosophy*.
8. **Time-for-space.** Many small precise corrections beat fewer batched
   ones. Each error caught during execution is a precision signal — never
   batch errors for "efficiency."
9. **Local graph repair, never bulk.** When the verifier finds issues, fix
   one at a time with a subgraph-scoped patch. Global rebuilds are an
   explicit opt-in, not an error path.
10. **Universality lives in the model, structure lives in the graph.** The
    harness is generic across domains; domain-specific judgment is delegated
    to the model. Don't put domain enums into shared types.
11. **Reviewer needs deterministic backstops.** LLM-as-judge is unreliable
    alone. Layer multiple deterministic checks (graph consistency, sub-agent
    success, post-execution validation) BEFORE trusting the model's verdict.
12. **Scanners are seeds, not the product.** Code/data/infra scanners
    produce low-confidence starter graphs (≤ 0.6). The model is the real
    graph builder; don't over-invest in scanner cleverness.

## Honest scope

What this is **NOT** (yet):

- A complete code-editing agent. Sub-agents have `bash` under a default
  `DangerousCommandDeny` policy (precise high-risk command patterns like
  `rm -rf /`, `kubectl delete`, `git push --force`) and an auto-derived
  `ScopeGuard` that restricts writes to paths reachable from the task's
  `involved_nodes`. Going beyond these bounds — for example, granting
  destructive commands or expanding the write scope — requires explicit
  `with_pattern` / `without_pattern` / custom-guard configuration.

- A full multi-agent framework with named roles, persistent memory, etc.
  Sub-agents are single-shot tool-calling loops; nested GraphLoops are
  reserved for a future iteration.

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

## License

Dual-licensed under MIT OR Apache-2.0 (see `Cargo.toml`).
