# Graph-Centric Agent Harness — CLAUDE.md

## Project identity

A universal LLM agent orchestrator in Rust (~15K LOC). Core thesis: **every agent task is fundamentally an operation on a relationship graph.** The graph is the orchestrator's plan — the LLM maintains it as working memory through Graph → Task → Review phases.

- **Model-agnostic**: speaks OpenAI-compatible HTTP (`/v1/chat/completions`)
- **State machine**: fixed FSM (Graph / Task / Review / Done), not free-form ReAct
- **Three-layer graph**: L0 (nodes+edges) / L1 (semantic descriptions) / L2 (raw source, on-demand)
- **Defense in depth**: every "trust the model" decision is checked at least twice (sub-agent + dispatcher; verifier + reviewer; graph self-check + post-execution validator). The hard gate is always deterministic.
- **Web gateway**: axum HTTP/WS server + Vue 3 + Vite frontend

## Core ideas

The system is built on a small number of load-bearing ideas. When changing code, check whether your change preserves them.

### 1. The relationship graph IS the plan, not a representation of one

The LLM doesn't write prose plans or call tools ad-hoc — it edits a `Graph` (nodes + edges + L1 descriptions) that's the orchestrator's working memory. `proposer.rs` accepts only step types that modify this graph (`propose_patch`, `explore`, `ask_user`, `ready_for_verify`, `block`, `consult_advisor`). Every other component (verifier, repairer, decomposer, dispatcher, reviewer) reads the same graph. There is no scratchpad, no hidden state, no "I thought about it for a while and decided..." outside the graph.

### 2. The FSM is code, not prompt

`Phase::{Graph, Task, Review, Done, Poisoned}` and `GraphPhase::{Clarifying, Seeding, Filling, Expanding, Verifying}` are Rust enums in `graph_loop.rs`. Transitions happen in `step()` based on the model's step + the verifier's verdict, not on whatever the model thinks comes next. The model's job is to emit one of the allowed step kinds; the loop's job is to enforce what each step kind actually does. This is the hard guarantee that "the agent doesn't wander off into infinite `ls` loops" — the FSM won't let it.

### 3. Two intake modes, gated by code

Round 0 has a gate (`intake.rs`): vague tasks must emit `ask_user` first, clear tasks may emit `propose_patch`. The classifier is heuristic (vague starter phrases EN+ZH, short + no verb, single word) and **errs on the side of letting through** (false positives annoy; false negatives waste the run). The gate is the second line of defense — the system prompt teaches Mode A/B too, but prompt-only is not load-bearing.

### 4. Sub-agent contracts are deterministic, not LLM-graded

Every sub-agent dispatch in `contract.rs::CheckContract` is one of: `KnowHow` (must mention expected phrases, min length), `Exploratory` (must stay within `region` + `max_items`), `MustEdit` (must make ≥1 write tool call), or `None`. The check runs inside the sub-agent (self-check before `final_answer`) AND in the dispatcher (second-line defense). LLM-as-judge is reserved for the final Review phase, never for per-sub-task verification. This is the **bounded-context invariant**: the main agent operates on L0/L1; the contract is the bridge that rejects L2 bleed-through.

A complementary guard is `ScopeGuard` in `subagent.rs::run`: every `use_tool` action is checked against an allowed-path policy **before** the tool is invoked. A sub-agent dispatched to "fix `auth.rs`" literally cannot write to `/var/log` or `~/.ssh/`. The bounded context is enforced at the **filesystem level**, not just the cognitive level.

### 5. Local graph repair, never bulk

`LocalRepairer` fixes one `VerifyIssue` at a time. `CascadeBacktracker` walks inbound edges one predecessor at a time. The FSM has no "discard the graph and start over" branch — graph repair is **monotonic within a run**. Every patch bumps `Graph::version` and is checkpointed, so any repair can be inspected or reverted. The cost: the agent can't globally restructure once it's committed. The benefit: the audit trail is real.

### 6. Skill capture closes the loop

When a run successfully reaches `ready_for_verify`, `skills::capture::capture_skill()` extracts the `propose_patch` sequence as a compiled task DAG and stores it locally (`LocalSkillStorage`). The next run with a token-Jaccard ≥ 0.25 task (`skills::matcher`) skips the decomposer entirely and uses the compiled skill graph directly. This is **structural memory** — the skill is a graph topology, not a prompt. Successful runs compound; the agent gets faster at things it's already done.

### 7. Drill-down expands complex nodes into sub-graphs

A `Task` node marked `[drill_down]` is the anchor of a sub-run (`fork_sub_graph_for`), capped at `max_drilldown_depth=2` (L0→L1→L2). The sub-run produces a sub-graph, which is folded back into the parent via the cascade-expansion step. This is how the agent keeps L0 tractable: each level only has to think about *this level's* abstraction, with the deep details farmed out to recursive sub-runs.

### 8. The verifier, reviewer, self-check, and validator are advisory, never the gate

The deterministic structural checks (`Graph::find_inconsistencies`, sub-agent success, verify-phase final status) are the hard gate. The LLM-as-judge layers (verifier's L1 sampling, graph self-check, reviewer's judge verdict) are advisory — they surface concerns but cannot unilaterally fail a run unless the structural layer agrees. `PostExecutionValidator` (in `validator.rs`) is the same pattern applied *between* Task and Review phases: it runs a configured command (e.g. `cargo check`) and **classifies** the failure rather than just detecting it. If the non-zero exit + stderr contains a "graph signature" like `"cannot find function"` / `"unresolved import"`, the verdict is `FailedAsGraphIssue` (sub-agent wrote code that the graph didn't say was valid); otherwise it's `FailedAsTaskIssue` (the sub-agent's work is wrong, but the graph was right). LLM-free failure attribution, pattern-matched against language-specific error templates. This is the **deterministic reviewer backstops** design principle: a flaky model can't take down a structurally sound graph.

### 9. Three-stream UI feedback: text, tool_call, end

`StreamChunk` (incremental text + reasoning), `StreamToolCall` (assembled or per-fragment tool_call args), `StreamEnd` (terminal). Frontend renders thinking blocks + tool-call timeline + final result. `RunEvent::StreamToolCall` with `type: "stream_tool_call"` is the wire shape — even the non-streaming `complete()` path emits it so the frontend can show "agent is calling X with..." in the timeline.

### 10. HeartBeat is autonomous improvement, not user-facing

`POST /api/heartbeat/default` starts a 10-round self-optimization loop. Each round runs the full Graph→Task→Review flow; the run that succeeds becomes the new baseline. State persists in `.graph_harness_heartbeat.json` so the loop survives process restarts. Errors count as learning (the next round gets a different prompt, not a panic).

### 11. Three orthogonal memory tiers

The system has three distinct, complementary "memories":

| Tier | Storage | Lifetime | What's in it |
|---|---|---|---|
| **Structural** (graph) | `Graph` in memory + checkpointed to disk | The run | L0 nodes/edges + L1 descriptions — the orchestrator's plan |
| **Prompt** (conversation) | `Conversation` in memory | The run | The LLM's chat history — what the model has seen, including ask_user exchanges, verifier rejections, repair attempts |
| **Compiled** (skills) | `LocalSkillStorage` (filesystem) | Permanent | Extracted task DAGs that worked, indexed by Jaccard-token similarity |

These three are **orthogonal**: skills don't leak into the graph, the graph doesn't leak into the conversation, the conversation doesn't leak into skills. Each is a separate data structure with its own write path. The integration point is `try_match_and_compile_skill` in the Task phase — a successful skill substitutes for the decomposer output but never modifies the graph or conversation. New "memory" features should pick a tier and a write path; resist the urge to put it everywhere.

### 12. `LoopState` is the entire public API

Despite `graph_loop.rs` being ~6.7K LOC, the entire public API of the run loop is **5 variants** in `LoopState` (`graph_loop.rs:60`): `Running` (continue stepping), `Paused` (waiting for `ask_user` answer), `GraphInvalid` (caller must repair), `Done` (terminal success), `Error` (terminal failure). The web gateway (`api_runs.rs`, `ws.rs`) only ever sees these 5; everything inside the loop is private. This is the discipline that lets the core be refactored freely without breaking the gateway.

### 13. The post-Explore commit gate

After a sub-agent `Explore` step completes, the *next* proposer step is constrained to one of: `ProposePatch` (commit findings to the graph), `Block` (declare a blocker), `AskUser` (clarify), or `ReadyForVerify` (declare the graph is done). Dispatching another `Explore` (or any other non-committing step) is rejected in `proposer.rs:492` with a specific error message. This is the **anti-infinite-explore guard** that killed the 602-round production run 2026-06-09. Without it, the model keeps dispatching sub-agents and never updates the graph; with it, every Explore must pay off into graph mutation or be declared a failure.

### 14. The sub-agent uses a simpler protocol than the main agent

The main agent speaks OpenAI `tool_calls` (structured, with full tool schemas). The sub-agent (`subagent.rs`) speaks a custom **JSON-action protocol** parsed from `resp.content`:

```json
{"action": "use_tool", "tool": "<name>", "args": {...}, "thinking": "<why>"}
{"action": "final_answer", "answer": "<result>", "thinking": "<why complete>"}
{"action": "report_graph_error", "errors": [...]}  // bubbles a GraphError to the parent
```

This isn't a missing tool_calls migration — it's a **deliberate split**. Sub-agents are constrained environments (max `max_steps=8`, no direct graph access, only `final_answer` is exposed to the dispatcher), so the simpler text-protocol is appropriate. The main agent's tool_calls is the "rich" interface; the sub-agent's JSON-action is the "narrow, audited" interface. Don't unify them — the narrow contract is what makes sub-agents cheap to constrain.

### 15. Reachability is enforced at `ready_for_verify`, not trusted

When the proposer emits `ready_for_verify`, the loop runs `replay_from_anchor` (a BFS from `start` over `LeadsTo`/`DependsOn`) and bounces the graph back to `Filling` if any node is unreachable. This is the **graph reachability gate**: a model can claim "I'm done" but cannot ship a graph where some node isn't actually on the path from start to deliverable. `GraphInvalid` from the verifier gets the same treatment at a different level. Trust the structure, not the model.

### 16. The graph IS the schedule — wave-aligned batch dispatch

`scheduler::DagScheduler::plan` runs a variant of Kahn's algorithm over `DependsOn` edges and produces a `Schedule { batches: Vec<Vec<NodeId>> }`. `Dispatcher::run` iterates the batches **in order** (`for batch in schedule.batches.iter().enumerate()`), but **within** each batch all sub-agents are spawned concurrently, throttled by a `Semaphore::new(max_concurrent)`. The graph's structure determines the concurrency — two independent tasks automatically land in the same wave; a dependent task waits for its prerequisites' wave to complete. Cycles are reported as errors, not silently truncated. **The dispatcher is dumb on purpose**: it doesn't invent scheduling, it just executes what the graph already encodes. When you read the graph's edge structure, you can predict exactly which waves will run in parallel and which will be serial.

### 17. Fail-soft within a batch, fail-collect across waves

A sub-agent that fails (model error, contract violation, `max_steps` exceeded) does **not** abort its siblings in the same batch — `dispatcher.rs:23` explicitly says "we collect all results, success or not. The caller decides what to do with failures." Across waves, the dispatcher awaits each batch sequentially before starting the next, so a failure in wave 0 is visible to the run loop before wave 1 starts. **tokio join errors** (panic in a spawned task) DO abort the batch — those are bugs, not sub-agent problems. The result is `DispatchOutcome { all_succeeded: bool, graph_errors: Vec<GraphError>, results: Vec<SubAgentResult> }` — a structured failure report, not an exception. The graph loop inspects it and routes to `GraphInvalid` (graph errors), `FailedAsGraphIssue` (validator), or continues to Review based on the failure shape.

### 18. Contracts are verified at two layers (defense in depth)

Every `SubTask` carries a `CheckContract`. The contract is checked **twice**:

1. **Sub-agent self-check** (`subagent.rs:run`) — the model gets a user-message rejection if its `final_answer` fails the contract, and is told to either retry or emit `report_graph_error`. This is the "inner loop" of contract enforcement.
2. **Dispatcher re-check** (`dispatcher.rs::run`, with `verify_contract: true` by default) — the dispatcher independently re-evaluates the contract after the sub-agent returns. This catches the (rare) case where the sub-agent's self-check disagrees with the dispatcher's view (e.g. the sub-agent exhausted `max_steps` without ever satisfying the contract, and returned `success: true` with a fallback answer).

Each layer assumes the other is buggy. `DispatcherConfig::with_verify_contract(false)` is **opt-in for tests only** — production runs leave it on. The pattern generalizes: any "trust the model" decision in this codebase is checked at least twice (sub-agent + dispatcher; verifier + reviewer; graph self-check + post-execution validator).

### 19. Skill compilation is a pure transformation

`skills::compiler::compile_skill_to_task_graph` is a pure function: same `Skill` in → same task graph out. No I/O, no model calls, no randomness. The mapping rules are explicit and minimal:

- L0 nodes → `NodeKind::Task` with id prefixed `skill:<slug>:<node_id>`
- L0 edges → `RelationType::DependsOn` (skill edges are always prerequisite)
- L1 descriptions → task summaries (fallback to L0 summary)
- `skill_slug` / `skill_trigger` / `skill_node_id` written into each task node's metadata for provenance

The output graph is fed **directly** into `DagScheduler` — the test `dag_is_schedulable` asserts this contract holds. **Skills are reproducible**: re-compiling the same skill always gives the same task DAG. The id prefix prevents collision with the host run's task graph, and the metadata makes "where did this task come from" a queryable property, not a comment. If you add a new kind of skill, the compile output must be a `Graph` of `NodeKind::Task` + `DependsOn` edges — that's the boundary the rest of the system relies on.

### 20. `CheckpointStore` is git-for-runs

`web::checkpoint::CheckpointStore` maintains an append-only log of `(round, phase, graph_snapshot, transcript)` triples, plus a `branches: HashMap<usize, Vec<String>>` map from checkpoint index → child run ids:

- `push(round, phase, graph, transcript)` — append a snapshot. If `persistence` is set, flushes to disk via `RunPersistence::save_checkpoint`.
- `create_branch(from_index, child_run_id)` — fork a new run from any historical checkpoint. The branches map is also persisted.
- The `transcript: Vec<Message>` is snapshotted too — replay includes the LLM's exact view, not just the graph.

This is **graph-versioned execution history**. The frontend (`api_runs.rs::GET /api/runs/:id/checkpoints`) can list checkpoints and let the user rewind, compare two paths, or fork an exploratory variant from an interesting mid-run state. The combination of `Graph::version` (bumped on every patch) + checkpoint snapshots + branching is the auditability foundation that idea #5 (local repair) and idea #12 (LoopState API) both rely on.

### 21. Git safety checkpoint is opt-in, never default

`SubAgentPool::auto_git_checkpoint` defaults to `false` (dispatcher.rs:64). The design intent is documented in the field's docstring: *"task execution should not mutate git history as a hidden side effect. Failed work is reported through the graph/result flow rather than being reverted with reset/checkout."* When the caller opts in, the pool does the minimum: on sub-agent success it runs `git add -A` + `git commit -m "🤖 subagent: <description>"` (truncated to 80 chars). On failure it does **not** auto-revert — the working tree is left as-is so a human can inspect what went wrong. This is the principle: **the run annotates itself in git history when it makes progress; it never silently erases work**. The default keeps the agent from being a black box that quietly rewrites your repo.

### 22. Narrow protocols at the boundary, rich protocols in the middle

Across three rounds of reading the code, one pattern keeps surfacing: **the system deliberately uses different protocols at different layers, narrowing the surface area as you go deeper.**

| Layer | Protocol | Shape | Why narrow |
|---|---|---|---|
| Main agent | OpenAI `tool_calls` | Rich (6 step types, full GraphPatch schema) | This is the orchestration surface — needs the most flexibility |
| Sub-agent | Custom JSON-action | Narrow (`use_tool` / `final_answer` / `report_graph_error`) | Constrained execution env (`max_steps=8`, no direct graph access); narrow = easy to verify |
| Skill compile | `NodeKind::Task` + `DependsOn` only | Narrower still (no L1 mutability, no relation choice) | Skills are cached, replayed, and trusted; the narrower the contract, the safer the cache |

The first instinct on encountering this is to **unify** — make the sub-agent also use `tool_calls`, make skills emit full `GraphPatch`es. Resist. Each narrowing is a **defense-in-depth decision**: the narrower the protocol at a boundary, the easier it is to validate at that boundary, and the smaller the blast radius if a model misbehaves at that layer. The main agent is the rich one because that's where the creative work happens; everything inside it is on rails. If a future contributor proposes unifying these protocols, the question to ask is: **what's the safety guarantee we'd lose?** If there's no good answer, keep them split.

## Build & run

```bash
# Build
cargo build --bin serve

# Run (serves on http://localhost:8080)
cargo run --bin serve

# Tests
cargo test --lib                    # 606 tests (post tool_calls migration)
cargo test --lib skills::matcher::  # skill matching tests only

# Frontend
cd webui && npm run dev             # dev server on :5173
cd webui && npm run build           # production build → webui/dist/

# Config
cp .env.example .env                # MODEL_BASE_URL, MODEL_API_KEY, etc.
```

## Architecture map

```
src/
├── agent/                # Core orchestrator
│   ├── graph_loop.rs     #   FSM: Graph → Task → Review → Done (Phase + GraphPhase enums)
│   ├── proposer.rs       #   Main-agent step engine (6 step types, tool_calls)
│   ├── decomposer.rs     #   World graph → task DAG via emit_task_decomposition tool
│   ├── enricher.rs       #   L2 → L1 via write_l1_description tool
│   ├── repairer.rs       #   Per-issue GraphPatch via propose_repair_patch tool
│   ├── verifier.rs       #   L1 drift + graph self-check (2 tool schemas)
│   ├── reviewer.rs       #   Final review via judge_verdict tool
│   ├── cascade.rs        #   Cascade backtracking via classify_predecessor_verdict tool
│   ├── dispatcher.rs     #   Wave-aligned batch dispatch (SubAgentPool + DagScheduler)
│   ├── subagent.rs       #   JSON-action ReAct loop + ScopeGuard + contract self-check
│   ├── validator.rs      #   PostExecutionValidator (cargo/tsc failure classifier)
│   ├── intake.rs         #   Mode A/B intake gate (vague → ask_user)
│   ├── contract.rs       #   CheckContract (KnowHow/Exploratory/MustEdit/None)
│   ├── cascade_expand.rs #   L0→L1→L2 cascade expansion in Task phase
│   ├── conversation.rs   #   LLM prompt history
│   └── mod.rs            #   agent module root
├── graph/                # L0/L1/L2 data model: Node, Edge, Graph, GraphPatch
├── scheduler/            # DagScheduler (Kahn-based wave decomposition)
├── model/                # Model trait + OpenAI-compatible HTTP client
│   ├── mod.rs            #   Model trait, StreamDelta, ModelResponse,
│   │                     #   ModelResponse::text_or_reasoning() helper
│   ├── openai_compat.rs  #   SSE streaming + tool_call fragment forwarding
│   └── streaming.rs      #   ModelWithEvents wrapper (forwards StreamDelta to RunEvent)
├── skills/               # Skill capture & reuse
│   ├── matcher.rs        #   Token-based skill scoring (Jaccard ≥ 0.25)
│   ├── compiler.rs       #   Pure Skill → task DAG transformation
│   ├── capture.rs        #   Auto-capture from successful runs
│   ├── retrieve.rs       #   list_for_prompt + find_and_load_matching_skills
│   ├── prompt_registry.rs #  Dynamic prompt blocks (Claude Code style)
│   ├── storage.rs        #   LocalSkillStorage (~/.local/share/...)
│   ├── storage_composite.rs
│   ├── storage_repo.rs
│   ├── slug.rs
│   └── types.rs
├── tools/                # Bash, ReadFile, WriteFile, EditFile, WebSearch, WebFetch,
│                         #   ScopeGuard (filesystem-level path policy)
├── web/                  # axum HTTP/WS server
│   ├── api_runs.rs       #   /api/runs/* + drive_run (the main run driver)
│   ├── ws.rs             #   WebSocket handler (/ws/runs/:id)
│   ├── events.rs         #   RunEvent enum (StreamChunk / StreamToolCall / StreamEnd / ...)
│   ├── heartbeat.rs      #   Self-improving loop across process lifetimes
│   ├── persistence.rs    #   Run persistence (data/runs/<id>/)
│   ├── checkpoint.rs     #   CheckpointStore + branching
│   ├── run_session.rs    #   Per-run session state
│   ├── config_api.rs     #   Runtime config endpoint
│   ├── mod.rs            #   web module root + router
│   └── state.rs          #   WebState, EngineConfig, LoopTuningConfig
└── bin/
    ├── serve.rs          #   Web gateway binary (main entry)
    └── agent_a.rs        #   CLI demo binary
```

## Key architectural decisions

1. **Model trait with streaming fallback**: `Model::complete()` is the primary API. `ModelWithEvents` wraps any model and transparently routes `complete()` through SSE `complete_stream()`. Fallback: non-streaming models get one big `Delta` + `Done`.

2. **Pure orchestrator**: main agent has NO direct tool access. Its only execution path is `explore` → dispatches subagents that have bash/file/web tools. This prevents `ls` loops.

3. **Two model tiers**: fast (Proposer, Verifier, SubAgent) vs deep (L1Enricher, Decomposer, Reviewer, CascadeBacktracker).

4. **Per-issue repair, never bulk**: `LocalRepairer` fixes one `VerifyIssue` at a time. CascadeBacktracker walks inbound edges one predecessor at a time. Graph repair is monotonic within a run; every patch bumps `Graph::version` and is checkpointed.

5. **Native tool_calls everywhere**: All 7 model layers (Proposer, Decomposer, L1Enricher, L0+ScopeGapRepairer, Verifier L1+graph self-checks, Reviewer, CascadeBacktracker) declare an OpenAI `tool` schema and prefer `tool_calls` over text-JSON parsing. Each layer's `parse_*_from_tool_calls()` returns `Option<T>`; on `None` the caller falls through to the text fallback. This is the only way to robustly handle DeepSeek / MiniMax M3 reasoning-only responses (where `content` is empty and the final JSON is in `reasoning_content`). See "Tool calls migration" below for the per-layer pattern.

6. **Skill auto-matching**: When a task matches a stored skill (Jaccard token overlap ≥ 0.25), the skill's compiled task DAG substitutes the decomposer output in the Task phase. Controlled by `auto_apply_skills` (default: true).

7. **Streaming output**: Every model call emits `StreamChunk` / `StreamToolCall` / `StreamEnd` events via WebSocket. Frontend renders `thinking` blocks + tool-call timeline + incremental `assistant_streaming` text.

8. **HeartBeat self-improvement**: `POST /api/heartbeat/default` starts a 10-round autonomous optimization loop. Each round: Explore → ProposePatch → SubAgent → Review. On success, auto-spawns next round. Survives restarts (`.graph_harness_heartbeat.json`).

9. **Deterministic backstops beat LLM-as-judge**: The hard gate is always deterministic (`Graph::find_inconsistencies`, sub-agent success flag, `replay_from_anchor` reachability BFS, contract `KnowHow`/`MustEdit` checks, `PostExecutionValidator` exit-code + stderr pattern-match). LLM-as-judge layers (verifier L1 sampling, graph self-check, reviewer) are advisory. A flaky model cannot take down a structurally sound graph. Any new "trust the model" decision must come with a deterministic second-line check.

10. **The graph IS the schedule**: `DagScheduler` runs Kahn over `DependsOn` edges and produces wave-aligned batches. The dispatcher's job is just to execute the waves; concurrency is determined by the graph structure, not invented by the dispatcher. Cycles are errors, not silent truncation. This is what makes the system "naturally parallel" without an explicit orchestration language.

11. **Full execution history is checkpointed and branchable**: `CheckpointStore` maintains an append-only log of `(round, phase, graph_snapshot, transcript)` triples, plus a `branches: HashMap<usize, Vec<String>>` map for forks. The frontend can rewind, replay, or fork a variant from any historical state. The combination of `Graph::version` + checkpoints + branches is the auditability foundation for local repair and the 5-variant `LoopState` public API.

12. **Narrow protocols at the boundary, rich in the middle**: Main agent uses rich OpenAI `tool_calls`; sub-agent uses narrow custom JSON-action; skill compile uses narrower still (Task + DependsOn only). The narrowing is intentional — each layer's narrow contract is what makes it easy to verify. Don't unify protocols across boundaries without asking "what safety guarantee do we lose?"

## JSON parsing robustness

`extract_json_block()` in proposer.rs is now the **text-fallback path**, not the primary parser. It handles model responses that mix thinking/prose with JSON:
- Strips `<think>...</think>` blocks (DeepSeek reasoning)
- Strips markdown code fences (` ```json ... ``` `)
- Finds ALL outermost `{...}` blocks (brace depth 0)
- Tries each candidate right-to-left, returns first valid JSON
- Works with or without thinking, with or without nested JSON examples in prose

The primary path is `parse_*_from_tool_calls()` on each layer (see "Tool calls migration"). If a model supports function calling, `tool_calls` carries the structured response and the text path is never reached.

## Tool calls migration

Every model layer follows the same pattern. New layers being added should copy this — **don't add a layer that only does text-JSON parsing**.

```rust
// In the layer's call site:
let req = ModelRequest {
    messages: vec![/* system + user prompt */],
    tools: vec![my_layer_tool_schema()],   // ← declare the schema
    temperature: /* ... */, max_tokens: /* ... */, stop: vec![],
};
let resp = self.model.complete(req).await?;

// Strategy A: prefer native tool_calls; fall back to text.
if let Some(v) = parse_my_layer_from_tool_calls(&resp.tool_calls) {
    return Ok(v);
}
// Reasoning-model fallback (DeepSeek / M3): see ModelResponse::text_or_reasoning.
let parse_text = resp.text_or_reasoning();
if parse_text.trim().is_empty() {
    return Err(HarnessError::model("my_layer: empty response — ...".into()));
}
parse_my_layer_from_text(parse_text)
```

Three pieces per layer (~50 LOC each):
1. `*_tool_schema()` — `serde_json::json!({...})` with the function name, description, and parameter schema.
2. `parse_*_from_tool_calls(&[ToolCall]) -> Option<T>` — `None` on missing tool_call or missing required field → caller falls back.
3. The existing `parse_*_from_text()` function — kept as the fallback, unchanged.

**Each layer's `*_tool_schema()` function is the source of truth for the wire shape.** Reuse the proposer's `propose_patch` schema for any layer that produces a `GraphPatch` (repairer, future replanner) to keep the wire shape consistent.

### Per-layer tool names (canonical reference)

| Layer | Tool name | Failure semantics on parse fail |
|---|---|---|
| Proposer | `propose_patch` / `explore` / `ask_user` / `ready_for_verify` / `block` / `consult_advisor` | retry 1x (existing `next_step_with_retry`) |
| Decomposer | `emit_task_decomposition` | clear "empty response" error |
| L1 Enricher | `write_l1_description` | existing `parse_l1_description` behavior |
| L0 Repairer + ScopeGap | `propose_repair_patch` | existing `parse_json` behavior |
| Verifier L1 self-check | `l1_check_verdict` | `continue` (silently skip node) |
| Verifier graph self-check | `graph_self_check_verdict` | `return (empty, 0.5)` (no model opinion) |
| Reviewer | `judge_verdict` | default-fail |
| Cascade probe | `classify_predecessor_verdict` | silent `Preserved` |

The "failure semantics" column matters: flaky models in the middle of a run shouldn't take the whole run down. Migration is **additive** — the text fallback still exists for every layer, and each layer preserves its pre-migration failure semantics.

### Streaming for tool_calls (frontend)

`StreamDelta` has a `ToolCallArgument` variant carrying `(index, id, name, arguments_fragment)` per OpenAI's `delta.tool_calls[i].function.arguments` shape. `openai_compat.rs::complete_stream` forwards each fragment as it arrives; the non-streaming `complete()` path emits one assembled variant per tool_call. The wire event is `RunEvent::StreamToolCall` with `type: "stream_tool_call"`, which the frontend in `RunView.vue` can render as "agent is calling X with…" in real time.

### `ModelResponse::text_or_reasoning()` helper

Reasoning-only models (DeepSeek-v3, MiniMax M3) return an empty `content` field and put the final JSON in `reasoning_content`. This helper centralizes the fallback: returns `content` if non-blank, else `reasoning_content`, else `""`. **Use it everywhere a layer reads the model's text response** — direct field access on `resp.content` is a regression risk. db2d993d's root cause was exactly this.

## HeartBeat state machine

```
State file: .graph_harness_heartbeat.json
API: POST /api/heartbeat/default  → start 10-round loop
     GET  /api/heartbeat          → status
     POST /api/heartbeat/cancel   → stop

Round lifecycle:
  1. serve.rs startup: loads state, if active && current_run_id is stale/null → spawn run
  2. drive_run Done → round_complete() → spawn_heartbeat_continuation()
  3. drive_run Error → round_complete() (error counts as learning) → spawn next round
  4. After max_rounds → active=false, loop stops
```

## Common issues & fixes

- **"no '{' in response"** / **"decomposer failed: model: proposer: no '{' in response"**: pre-tool_calls symptom of a reasoning-only model response. As of the tool_calls migration, the 7 model layers all prefer `tool_calls` first and only fall back to text parsing on `None`. If you still see this error, it means: (a) a new layer was added that doesn't follow the tool_calls pattern (see "Tool calls migration"); (b) the model genuinely doesn't support function calling — check the backend's `tools` support; (c) the prompt is asking the model to refuse / use a different tool name. The error should now be **rare** rather than the default failure mode. The fix when it does happen is: declare a `*_tool_schema()` for the affected layer, write a `parse_*_from_tool_calls()`, and replace the direct `resp.content` access with `resp.text_or_reasoning()`.
- **"fix-it retry did not converge"** (proposer only): both the initial attempt AND the retry failed to parse. The retry path is in `next_step_with_retry`. If this happens often, the proposer tool schema's `description` field may be unclear — model is calling the right tool with garbage args. Tighten the description and add an `enum` constraint where possible.
- **Dispatcher reports `all_succeeded: false` for a single contract violation**: this is the **double-check** (#18) working as designed. The sub-agent's self-check should have caught the contract first; if it didn't, the dispatcher's re-check does. Inspect which layer missed: look at the sub-agent's max_steps log (did it retry?) and the dispatcher's `results[].error` (does it say "contract violated" or "max_steps"?). Either way, the run correctly failed-soft instead of accepting bad work.
- **Sub-agent reports a `GraphError` mid-batch**: the dispatcher aggregates them into `DispatchOutcome.graph_errors`; the graph loop surfaces `LoopState::GraphInvalid { source: DuringExecution }`. Don't paper over with auto-retry — the graph is wrong, repair it.
- **Reachability check bounces back to `Filling` after `ready_for_verify`**: some node isn't on a `LeadsTo` path from `start` to `deliverable`. Inspect the orphan list in the warning log; either add a `LeadsTo` edge or remove the orphan.
- **Server won't bind**: `serve.exe` from a previous run may still be holding port 8080. `Stop-Process -Name serve -Force`.
- **Heartbeat stopped**: check `.graph_harness_heartbeat.json`. If `current_run_id` points to a zombie, restart the server — the startup logic detects and replaces it.
- **A new model layer is added without tool_calls support**: regression. Add a `*_tool_schema()` + `parse_*_from_tool_calls()` per the "Tool calls migration" section. Direct `resp.content` access is the regression risk; route through `text_or_reasoning()` even on the text path.

## When adding new features

- New `ModelResponse`/`Usage`/`SubTask`/`RunMetadata` fields: add `#[serde(default)]` for backward compat, update ALL struct literals in `#[cfg(test)]` blocks.
- New `GraphLoopConfig` fields: update `defaults_at()`, all struct-literal constructors in `agent_a.rs`, `api_runs.rs`.
- New `LoopTuningConfig` fields: add `#[serde(default = "...")]` + update `EngineConfig::default()`.
- New web routes: add to `src/web/mod.rs` router.
- New prompt blocks: register in `PromptRegistry::new()` and add to `compose()` ordering.
- **New model layer that calls `self.model.complete(...)`**: must declare a `*_tool_schema()`, add a `parse_*_from_tool_calls()` returning `Option<T>`, and use the `text_or_reasoning()` helper for the text-fallback path. Do NOT add a layer that only does text-JSON parsing — that's the regression we're protecting against. See "Tool calls migration" above.
- **New sub-agent tool or capability**: go through `ScopeGuard` (filesystem-level policy) and `CheckContract` (text-level predicate). The narrow JSON-action protocol is the contract boundary; widening it (e.g. adding a new `action` variant) requires updating `parse_action` + all sub-agent prompts + the contract checker. Don't bypass.
- **New `Skill` shape**: must compile to `NodeKind::Task` + `DependsOn` edges via `skills::compiler::compile_skill_to_task_graph` (a pure function). The compiled DAG must be `DagScheduler`-schedulable. Skill metadata (`skill_slug` / `skill_trigger` / `skill_node_id`) is part of the wire contract — preserve it.
- **New checkpoint-affecting change**: every patch that mutates the graph should bump `Graph::version` and trigger a `CheckpointStore::push`. The auditability story (ideas #5, #11, #20) depends on the history being complete.
