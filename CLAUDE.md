# Graph-Centric Agent Harness — CLAUDE.md

## Project identity

A universal LLM agent orchestrator in Rust (~15K LOC). Core thesis: **every agent task is fundamentally an operation on a relationship graph.** The graph is the orchestrator's plan — the LLM maintains it as working memory through Graph → Task → Review phases.

- **Model-agnostic**: speaks OpenAI-compatible HTTP (`/v1/chat/completions`)
- **State machine**: fixed FSM (Graph / Task / Review / Done), not free-form ReAct
- **Three-layer graph**: L0 (nodes+edges) / L1 (semantic descriptions) / L2 (raw source, on-demand)
- **Web gateway**: axum HTTP/WS server + Vue 3 + Vite frontend

## Build & run

```bash
# Build
cargo build --bin serve

# Run (serves on http://localhost:8080)
cargo run --bin serve

# Tests
cargo test --lib                    # ~477 tests
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
│   ├── graph_loop.rs#   FSM: Graph → Task → Review → Done
│   ├── cascade.rs   #   Cascade backtracking on sub-agent failure
│   └── subagent.rs  #   Tool-calling sub-agent (JSON-action protocol)
├── graph/           # L0/L1/L2 data model: Node, Edge, Graph, GraphPatch
├── model/           # Model trait + OpenAI-compatible HTTP client
│   ├── mod.rs       #   Model trait, StreamDelta, ModelResponse
│   ├── openai_compat.rs  # SSE streaming + retry logic
│   └── streaming.rs #   ModelWithEvents wrapper
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

5. **Native tool_calls preferred**: Proposer prefers structured `tool_calls` (no JSON parsing errors). Falls back to `parse_step()` for models without function-calling.

6. **Skill auto-matching**: When a task matches a stored skill (Jaccard token overlap ≥ 0.25), the skill's compiled task DAG substitutes the decomposer output in the Task phase. Controlled by `auto_apply_skills` (default: true).

7. **Streaming output**: Every model call emits `StreamChunk`/`StreamEnd` events via WebSocket. Frontend renders `thinking` blocks + incremental `assistant_streaming` text.

8. **HeartBeat self-improvement**: `POST /api/heartbeat/default` starts a 10-round autonomous optimization loop. Each round: Explore → ProposePatch → SubAgent → Review. On success, auto-spawns next round. Survives restarts (`.graph_harness_heartbeat.json`).

## JSON parsing robustness

`extract_json_block()` in proposer.rs handles model responses that mix thinking/prose with JSON:
- Strips `<think>...</think>` blocks (DeepSeek reasoning)
- Strips markdown code fences (` ```json ... ``` `)
- Finds ALL outermost `{...}` blocks (brace depth 0)
- Tries each candidate right-to-left, returns first valid JSON
- Works with or without thinking, with or without nested JSON examples in prose

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

- **"no '{' in response"**: model returned plain text without JSON. Fixed by native tool_calls + robust `extract_json_block`. If it happens on a model without tool_calls support, the response likely has thinking content; check the model's raw output.
- **"fix-it retry did not converge"**: both the initial attempt AND the retry failed to parse. Usually a deeper issue (model refuses to output JSON at all).
- **Server won't bind**: `serve.exe` from a previous run may still be holding port 8080. `Stop-Process -Name serve -Force`.
- **Heartbeat stopped**: check `.graph_harness_heartbeat.json`. If `current_run_id` points to a zombie, restart the server — the startup logic detects and replaces it.

## When adding new features

- New `ModelResponse`/`Usage`/`SubTask`/`RunMetadata` fields: add `#[serde(default)]` for backward compat, update ALL struct literals in `#[cfg(test)]` blocks.
- New `GraphLoopConfig` fields: update `defaults_at()`, all struct-literal constructors in `agent_a.rs`, `api_runs.rs`.
- New `LoopTuningConfig` fields: add `#[serde(default = "...")]` + update `EngineConfig::default()`.
- New web routes: add to `src/web/mod.rs` router.
- New prompt blocks: register in `PromptRegistry::new()` and add to `compose()` ordering.
