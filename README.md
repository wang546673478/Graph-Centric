# Graph-Centric Agent Harness

A Rust implementation of a universal LLM agent orchestrator built around one
thesis: **every agent task is fundamentally an operation on a relationship
graph.** Domain knowledge — code, infrastructure, research, planning — enters
through a single seam; the orchestrator stays generic.

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
  zero runtime dependencies, 310 unit/integration tests.
- **Model-agnostic.** Speaks OpenAI-compatible HTTP — works with DeepSeek,
  vLLM, Ollama, OpenAI, OpenRouter, Anthropic-via-proxy, or anything that
  serves `/v1/chat/completions`.
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

1. Builds a relationship graph through conversation (asks you clarifying
   questions when needed).
2. Decomposes the task into sub-tasks based on the graph.
3. Dispatches sub-agents concurrently (each with `bash` tool access under a
   read-only policy).
4. Runs `cargo check` as a post-execution validator (configurable).
5. Reviews the result with deterministic checks + LLM-as-judge.

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
| `scheduler::` | Topological batch scheduling | `DagScheduler`, `Schedule` |
| `context::` | Sub-agent context assembly | `ContextBuilder`, `ContextBudget`, `SourceLoader`, `FilesystemSources`, `InMemorySources`, `NullSourceLoader` |
| `model::` | Model abstraction + OpenAI-compat client | `Model` trait, `OpenAICompatModel`, `ModelConfig`, `Message`, `ModelRequest`, `ModelResponse` |
| `tools::` | Tool surface + Bash execution | `Tool` trait, `ToolRegistry`, `ToolDef`, `ToolOutput`, `ToolContext`, `Policy` (`AllowAll`/`ReadOnly`/`AllowList`), `BashTool`, `truncate_tail` |
| `agent::conversation` | Multi-turn dialog state | `Conversation` |
| `agent::proposer` | Builds the graph through model-emitted JSON steps | `GraphProposer`, `ProposerStep` (`AskUser`, `CallTool`, `ProposePatch`, `ReadyForVerify`) |
| `agent::verifier` | Three-layer verification (structural + model self-check + L1 sampling) | `Verifier`, `VerifyIssue`, `VerificationResult`, `Severity` |
| `agent::repairer` | Scope-bounded local repair (L0Structural / L1Semantic / ScopeGap) | `LocalRepairer` |
| `agent::enricher` | Model reads L2, writes L1 | `L1Enricher` |
| `agent::decomposer` | Model breaks task into sub-task DAG | `Decomposer` |
| `agent::subagent` | Single sub-task executor with tool-calling loop | `SubAgent`, `SubTask`, `SubAgentResult` |
| `agent::dispatcher` | Concurrent batch execution | `Dispatcher`, `SubAgentPool`, `DispatchOutcome` |
| `agent::reviewer` | Final acceptance gate | `Reviewer`, `ReviewResult`, `JudgeVerdict`, `RootCause` |
| `agent::validator` | Post-execution validator (between Task and Review) | `PostExecutionValidator`, `ValidationVerdict`, `BashCheckValidator` |
| `agent::graph_loop` | The state machine | `GraphLoop`, `LoopState`, `GraphError`, `L0ErrorType`, `ErrorSource`, `FinalResult` |
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
| 4 (review + bubble + auto-repair + validator) | done | Reviewer (det. + LLM judge), sub-agent `report_graph_error` action, Demo A auto-repair, extended ReadOnly whitelist, PostExecutionValidator + `BashCheckValidator` |

## What's runnable now

| Binary | Command | Purpose |
|---|---|---|
| `probe_model` | `cargo run --bin probe_model` | Backend connectivity smoke; prints latency + tokens per tier |
| `agent_a` | `cargo run --bin agent_a -- "<task>"` | Domain-agnostic main-agent demo (Phase 3 + 4 enabled) |
| `demo` | `cargo run --bin demo` | Phase 1 deterministic scanner demo (scans `./src` into a graph) |
| `graph_harness` | `cargo run --bin graph_harness` | Minimal Phase 1 smoke (constructs a tiny graph and prints it) |

## Tests

```bash
cargo test --lib                 # all 310 unit + integration tests
cargo test --lib agent::         # only agent layer
cargo test --lib tools::bash::   # only bash tool
cargo test --lib graph::         # only graph types
```

Live model tests are gated behind `LIVE_MODEL_TEST=1`:

```bash
LIVE_MODEL_TEST=1 cargo test --lib model::openai_compat
```

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
│   ├── mod.rs                 # Model trait, Message, ModelRequest, ModelResponse
│   ├── openai_compat.rs       # OpenAI-compatible HTTP client (reqwest)
│   └── config.rs              # ModelConfig — env-driven tiered loading
├── tools/
│   ├── mod.rs                 # Tool trait, ToolRegistry, Policy, ToolContext, truncate_tail
│   └── bash.rs                # BashTool with classify_read_only + ReadOnly-policy whitelist
├── agent/
│   ├── mod.rs                 # agent-layer re-exports
│   ├── conversation.rs        # Multi-turn dialog state
│   ├── proposer.rs            # Model emits 4 JSON step kinds
│   ├── verifier.rs            # Three-layer verification
│   ├── repairer.rs            # Scope-bounded local patch generation (3 paths)
│   ├── enricher.rs            # Model reads L2 -> writes L1
│   ├── decomposer.rs          # Model -> task DAG (NodeKind::Task + DependsOn edges)
│   ├── subagent.rs            # Single sub-task executor with tool-calling loop
│   ├── dispatcher.rs          # Concurrent batch execution (SubAgentPool + Dispatcher)
│   ├── reviewer.rs            # Deterministic + LLM-as-judge acceptance gate
│   ├── validator.rs           # PostExecutionValidator trait + BashCheckValidator
│   └── graph_loop.rs          # The state machine
├── domain/
│   ├── mod.rs                 # Domain, Scanner, ToolRegistry trait, DomainValidator, TaskNeeds
│   └── code/                  # Example domain: code-project scanner stub
└── bin/
    ├── probe_model.rs         # Backend connectivity smoke
    ├── demo.rs                # Phase 1 scanner demo
    └── agent_a.rs             # Main agent demo (Phase 3 + 4)
```

## Design principles

These shape every component:

1. **Model-agnostic.** Never hardcode a model name in source; all model
   selection flows through `ModelConfig` reading env.
2. **Time-for-space.** Many small precise corrections beat fewer batched
   ones. Each error caught during execution is a precision signal — never
   batch errors for "efficiency."
3. **Local graph repair, never bulk.** When the verifier finds issues, fix
   one at a time with a subgraph-scoped patch. Global rebuilds are an
   explicit opt-in, not an error path.
4. **Universality lives in the model, structure lives in the graph.** The
   harness is generic across domains; domain-specific judgment is delegated
   to the model. Don't put domain enums into shared types.
5. **Reviewer needs deterministic backstops.** LLM-as-judge is unreliable
   alone. Layer multiple deterministic checks (graph consistency, sub-agent
   success, post-execution validation) BEFORE trusting the model's verdict.
6. **Scanners are seeds, not the product.** Code/data/infra scanners
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
- A production retry / backoff layer for model calls. The
  `OpenAICompatModel` does exactly one HTTP call per `complete()`; rate
  limits or transient failures bubble up as `HarnessError::Model`.
- A persistence layer. Graphs serialize to JSON via `Graph::to_json`, but
  there's no built-in session store, checkpointing, or resume-across-process.

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
