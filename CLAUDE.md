# Graph-Centric Agent Harness — CLAUDE.md

## Project identity

A universal LLM agent orchestrator in Rust (~15K LOC). Core thesis: **every agent task is fundamentally an operation on a relationship graph.** The graph is the orchestrator's plan — the LLM maintains it as working memory through Graph → Task → Review phases.

- **Model-agnostic**: speaks OpenAI-compatible HTTP (`/v1/chat/completions`)
- **State machine**: fixed FSM (Graph / Task / Review / Done), not free-form ReAct
- **Three-layer graph**: L0 (nodes+edges) / L1 (semantic descriptions) / L2 (raw source, on-demand)
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

### 5. Local graph repair, never bulk

`LocalRepairer` fixes one `VerifyIssue` at a time. `CascadeBacktracker` walks inbound edges one predecessor at a time. The FSM has no "discard the graph and start over" branch — graph repair is **monotonic within a run**. Every patch bumps `Graph::version` and is checkpointed, so any repair can be inspected or reverted. The cost: the agent can't globally restructure once it's committed. The benefit: the audit trail is real.

### 6. Skill capture closes the loop

When a run successfully reaches `ready_for_verify`, `skills::capture::capture_skill()` extracts the `propose_patch` sequence as a compiled task DAG and stores it locally (`LocalSkillStorage`). The next run with a token-Jaccard ≥ 0.25 task (`skills::matcher`) skips the decomposer entirely and uses the compiled skill graph directly. This is **structural memory** — the skill is a graph topology, not a prompt. Successful runs compound; the agent gets faster at things it's already done.

### 7. Drill-down expands complex nodes into sub-graphs

A `Task` node marked `[drill_down]` is the anchor of a sub-run (`fork_sub_graph_for`), capped at `max_drilldown_depth=2` (L0→L1→L2). The sub-run produces a sub-graph, which is folded back into the parent via the cascade-expansion step. This is how the agent keeps L0 tractable: each level only has to think about *this level's* abstraction, with the deep details farmed out to recursive sub-runs.

### 8. The verifier, reviewer, and self-check are advisory, never the gate

The deterministic structural checks (`Graph::find_inconsistencies`, sub-agent success, verify-phase final status) are the hard gate. The LLM-as-judge layers (verifier's L1 sampling, graph self-check, reviewer's judge verdict) are advisory — they surface concerns but cannot unilaterally fail a run unless the structural layer agrees. This is the **deterministic reviewer backstops** design principle: a flaky model can't take down a structurally sound graph.

### 9. Three-stream UI feedback: text, tool_call, end

`StreamChunk` (incremental text + reasoning), `StreamToolCall` (assembled or per-fragment tool_call args), `StreamEnd` (terminal). Frontend renders thinking blocks + tool-call timeline + final result. `RunEvent::StreamToolCall` with `type: "stream_tool_call"` is the wire shape — even the non-streaming `complete()` path emits it so the frontend can show "agent is calling X with..." in the timeline.

### 10. HeartBeat is autonomous improvement, not user-facing

`POST /api/heartbeat/default` starts a 10-round self-optimization loop. Each round runs the full Graph→Task→Review flow; the run that succeeds becomes the new baseline. State persists in `.graph_harness_heartbeat.json` so the loop survives process restarts. Errors count as learning (the next round gets a different prompt, not a panic).

## Build & run

```bash
# Build
cargo build --bin serve

# Run (serves on http://localhost:8080)
cargo run --bin serve

# Tests
cargo test --lib                    # ~606 tests (post tool_calls migration)
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
├── agent/           # Core loop: GraphLoop, Proposer, Verifier, Reviewer,
│   │                #   Decomposer, Dispatcher, SubAgent, CascadeBacktracker
│   ├── proposer.rs  #   Model-driven step engine (ask_user/propose_patch/explore/...)
│   ├── decomposer.rs#   World graph → task DAG via emit_task_decomposition tool
│   ├── enricher.rs  #   L2 → L1 via write_l1_description tool
│   ├── repairer.rs  #   Per-issue GraphPatch via propose_repair_patch tool
│   ├── verifier.rs  #   L1 drift + graph self-check via l1_check_verdict /
│   │                #     graph_self_check_verdict tools
│   ├── reviewer.rs  #   Final review via judge_verdict tool
│   ├── cascade.rs   #   Cascade backtracking via classify_predecessor_verdict tool
│   ├── graph_loop.rs#   FSM: Graph → Task → Review → Done
│   └── subagent.rs  #   Tool-calling sub-agent (JSON-action protocol)
├── graph/           # L0/L1/L2 data model: Node, Edge, Graph, GraphPatch
├── model/           # Model trait + OpenAI-compatible HTTP client
│   ├── mod.rs       #   Model trait, StreamDelta, ModelResponse,
│   │                #   ModelResponse::text_or_reasoning() helper
│   ├── openai_compat.rs  # SSE streaming + tool_call fragment forwarding
│   └── streaming.rs #   ModelWithEvents wrapper (forwards StreamDelta to RunEvent)
├── skills/          # Skill capture & reuse
│   ├── matcher.rs   #   Token-based skill scoring (Jaccard)
│   ├── compiler.rs  #   Skill → task DAG compiler
│   ├── capture.rs   #   Auto-capture from successful runs
│   ├── retrieve.rs  #   list_for_prompt + find_and_load_matching_skills
│   ├── prompt_registry.rs  # Dynamic prompt blocks (Claude Code style)
│   └── storage.rs   #   LocalSkillStorage (~/.local/share/...)
├── tools/           # Bash, ReadFile, WriteFile, EditFile, WebSearch, WebFetch
├── web/             # axum HTTP/WS server
│   ├── api_runs.rs  #   /api/runs/* + drive_run (the main run driver)
│   ├── ws.rs        #   WebSocket handler (/ws/runs/:id)
│   ├── events.rs    #   RunEvent enum (StreamChunk / StreamToolCall / StreamEnd / ...)
│   ├── heartbeat.rs #   Self-improving loop across process lifetimes
│   ├── persistence.rs # Run persistence (data/runs/<id>/)
│   ├── checkpoint.rs  # CheckpointStore + branching
│   └── state.rs     #   WebState, EngineConfig, LoopTuningConfig
└── bin/
    ├── serve.rs     # Web gateway binary (main entry)
    └── agent_a.rs   # CLI demo binary
```

## Key architectural decisions

1. **Model trait with streaming fallback**: `Model::complete()` is the primary API. `ModelWithEvents` wraps any model and transparently routes `complete()` through SSE `complete_stream()`. Fallback: non-streaming models get one big `Delta` + `Done`.

2. **Pure orchestrator**: main agent has NO direct tool access. Its only execution path is `explore` → dispatches subagents that have bash/file/web tools. This prevents `ls` loops.

3. **Two model tiers**: fast (Proposer, Verifier, SubAgent) vs deep (L1Enricher, Decomposer, Reviewer, CascadeBacktracker).

4. **Per-issue repair, never bulk**: `LocalRepairer` fixes one `VerifyIssue` at a time.

5. **Native tool_calls everywhere**: All 7 model layers (Proposer, Decomposer, L1Enricher, L0+ScopeGapRepairer, Verifier L1+graph self-checks, Reviewer, CascadeBacktracker) declare an OpenAI `tool` schema and prefer `tool_calls` over text-JSON parsing. Each layer's `parse_*_from_tool_calls()` returns `Option<T>`; on `None` the caller falls through to the text fallback. This is the only way to robustly handle DeepSeek / MiniMax M3 reasoning-only responses (where `content` is empty and the final JSON is in `reasoning_content`). See "Tool calls migration" below for the per-layer pattern.

6. **Skill auto-matching**: When a task matches a stored skill (Jaccard token overlap ≥ 0.25), the skill's compiled task DAG substitutes the decomposer output in the Task phase. Controlled by `auto_apply_skills` (default: true).

7. **Streaming output**: Every model call emits `StreamChunk`/`StreamEnd` events via WebSocket. Frontend renders `thinking` blocks + incremental `assistant_streaming` text.

8. **HeartBeat self-improvement**: `POST /api/heartbeat/default` starts a 10-round autonomous optimization loop. Each round: Explore → ProposePatch → SubAgent → Review. On success, auto-spawns next round. Survives restarts (`.graph_harness_heartbeat.json`).

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
