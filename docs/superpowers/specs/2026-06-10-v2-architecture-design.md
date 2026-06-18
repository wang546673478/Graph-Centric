# v2 Architecture Implementation Spec

**Status:** Approved  
**Date:** 2026-06-10  
**Scope:** Cascade backtracking engine + WebSocket/Web rework + Checkpoint/branch system

## Overview

Replace SSE with WebSocket. Add cascade backtracking on sub-agent failure.
Rewrite web UI with Vue 3 + Vite. Add checkpoint/branch system for conversation
forking and history browsing. Config API for runtime model/tool/policy changes.

---

## 1. Communication Layer

### 1.1 REST Endpoints

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/api/health` | Health check |
| GET | `/api/config` | Current engine config |
| POST | `/api/config` | Hot-reload model/policy/tool config |
| GET | `/api/runs` | List all historical runs |
| POST | `/api/runs` | Create new run |
| GET | `/api/runs/:id` | Run metadata |
| DELETE | `/api/runs/:id` | Cancel running run |
| GET | `/api/runs/:id/checkpoints` | List checkpoints for a run |
| GET | `/api/runs/:id/checkpoints/:idx` | Full checkpoint snapshot |
| POST | `/api/runs/:id/branch` | Create branch from checkpoint |
| GET | `/api/skills` | List captured skills |
| GET | `/api/skills/:slug` | Get skill detail |
| DELETE | `/api/skills/:slug` | Delete skill |
| POST | `/api/skills/:slug/promote` | Promote skill |
| GET | `/api/files/changed` | List changed files |
| GET | `/api/files/diff` | Get file diff |

### 1.2 WebSocket: `GET /ws/runs/:id`

Upgrades to WebSocket. Per-run bidirectional channel.

**Client → Server:**
```json
{"type":"resume","answer":"..."}
{"type":"repair","graph":{...}}
{"type":"set_detail_mode","enabled":true}
```

**Server → Client:**
```json
{"type":"transcript","data":{"role":"...","content":"..."}}
{"type":"graph_snapshot","data":{"nodes":[...],"edges":[...]}}
{"type":"model_call","data":{"request":{...},"response":{...},"duration_ms":123}}
{"type":"cascade_step","data":{"node":"...","predecessor":"...","verdict":"preserved|needs_repair|needs_reexec","rationale":"..."}}
{"type":"checkpoint","data":{"index":17,"phase":"task","node_count":12,"edge_count":15}}
{"type":"status","data":{"phase":"...","message":"...","tokens_used":12345}}
{"type":"stream_chunk","data":{"component":"fast|deep","content":"...","reasoning_content":"...","finish_reason":null}}
{"type":"stream_end","data":{"component":"fast|deep","finish_reason":"stop|tool_calls|...","prompt_tokens":123,"completion_tokens":456}}
{"type":"done","data":{...}}
{"type":"error","data":{"message":"..."}}
```

`model_call` and `cascade_step` events are filtered when `detail_mode` is
`false` — the client tracks this toggle locally and sends `set_detail_mode`
to the server so it can skip serialization entirely for those event types
when detail mode is off (saving bandwidth and CPU).

### 1.3 New Rust Modules

```
src/web/
├── mod.rs              # Router, WebState (updated)
├── ws.rs               # NEW: WebSocket handler, WsConnection
├── checkpoint.rs       # NEW: Checkpoint, CheckpointStore
├── config_api.rs       # NEW: GET/POST /api/config
├── errors.rs           # (existing, extended)
├── state.rs            # (existing, extended with EngineConfig)
├── events.rs           # (existing, extended with new event variants)
├── run_session.rs      # (existing, extended with CheckpointStore)
├── api_runs.rs         # (existing, WS upgrade endpoint replaces SSE)
├── api_skills.rs       # (existing)
└── api_files.rs        # (existing)
```

---

## 2. v2 Engine — Cascade Backtracking

### 2.1 Graph Type Changes

```rust
// src/graph/mod.rs
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    pub path: Option<String>,
    pub summary: String,
    pub metadata: HashMap<String, String>,
    pub immutable: bool,  // NEW: true for anchor node A
}

impl Graph {
    /// All edges where node is the target (inbound edges).
    pub fn predecessors_of(&self, node: &NodeId) -> Vec<(&Edge, &Node)>;

    /// Walk inbound edges from node toward the anchor. Returns the
    /// ordered path from farthest ancestor to node (excludes anchor).
    pub fn path_to_anchor(&self, node: &NodeId) -> Vec<NodeId>;

    /// Mark a node as the immutable anchor. Only callable once per graph.
    pub fn set_anchor(&mut self, node: NodeId);
    pub fn anchor(&self) -> Option<&Node>;
}
```

### 2.2 CascadeBacktracker

New file: `src/agent/cascade.rs`

```rust
pub struct CascadeBacktracker {
    pub model: Arc<dyn Model>,       // deep model for verification decisions
    pub max_depth: usize,            // safety cap on backtrack distance
}

pub struct CascadeResult {
    pub preserved: Vec<NodeId>,      // design + output still valid
    pub needs_repair: Vec<NodeId>,   // design needs re-planning
    pub needs_reexec: Vec<NodeId>,   // design ok, output stale
}

pub enum PredecessorVerdict {
    Preserved,                       // design + output both valid
    NeedsRepair(String),             // design invalid, reason
    NeedsReexecution(String),        // design valid, output stale, reason
}

impl CascadeBacktracker {
    /// Entry point. Called after a node K is fixed/replaced by K'.
    /// Walks all inbound edges from K', verifies each predecessor,
    /// recurses on failures, stops at anchor.
    pub async fn backtrack_from(
        &self,
        changed_node: &NodeId,
        graph: &Graph,
        task: &str,
        l2_loader: &dyn SourceLoader,
    ) -> CascadeResult;

    /// Ask the model: does predecessor P still satisfy successor S's
    /// input requirements after S was redesigned?
    async fn verify_predecessor(
        &self,
        predecessor: &Node,
        successor: &Node,
        graph: &Graph,
        task: &str,
    ) -> PredecessorVerdict;
}
```

### 2.3 GraphLoop State Machine Changes

```rust
// src/agent/graph_loop.rs

pub struct GraphLoopConfig {
    pub max_rounds: usize,           // was 50 → now 300
    pub max_repair_rounds: usize,    // 4
    pub tool_cwd: PathBuf,
    pub tool_output_cap: usize,
    pub tool_policy: Arc<dyn Policy>,
    pub cascade_backtrack: bool,     // NEW: false = v1 behavior
}

impl GraphLoop {
    // NEW field
    cascade: Option<CascadeBacktracker>,

    // Modified: on sub-agent report_graph_error during Task phase,
    // feed failure back to Proposer for automatic re-planning,
    // then trigger cascade backtracking instead of surfacing
    // GraphInvalid to the caller.
    async fn handle_task_phase_graph_error(
        &mut self,
        errors: Vec<GraphError>,
    ) -> LoopState;

    // Modified: expose anchor ambiguity events for user clarification.
    // Other GraphInvalid sources are now handled internally.
}
```

**Key behavioral change:** `GraphInvalid` is no longer surfaced to the caller
for `DuringExecution`, `PostExecutionValidation`, or `Review` sources. Only
`VerifierStalemate` (repair budget exhausted) and anchor-level contradictions
reach the user. Everything else is handled by auto-replan + cascade backtrack.

### 2.4 300-Round Budget

```rust
// At the top of step():
if self.round >= self.config.max_rounds {
    // 1. Emit detailed checkpoint with:
    //    - full graph state
    //    - succeeded nodes vs failed nodes
    //    - last failure evidence
    //    - request for user guidance
    // 2. Transition to Paused { question: "budget exhausted, what now?" }
    // 3. User can: resume with new task description, or terminate
}
```

---

## 3. Frontend — Vue 3 + Vite

### 3.1 Project Structure

```
webui/
├── package.json
├── vite.config.ts
├── index.html
├── src/
│   ├── main.ts                  # App entry, router + mount
│   ├── App.vue                  # Root layout
│   ├── router.ts                # Vue Router config
│   │
│   ├── composables/
│   │   ├── useRun.ts            # Run state management
│   │   ├── useRunSocket.ts      # WebSocket connection + reconnection
│   │   ├── useRuns.ts           # Run list (REST)
│   │   ├── useConfig.ts         # Config CRUD (REST)
│   │   └── useCheckpoints.ts    # Checkpoint list + branch creation
│   │
│   ├── components/
│   │   ├── layout/
│   │   │   ├── AppLayout.vue    # Sidebar + main
│   │   │   └── Sidebar.vue      # RunList + CheckpointTree
│   │   │
│   │   ├── run/
│   │   │   ├── RunView.vue      # Main run page (route: /)
│   │   │   ├── Transcript.vue   # Virtual-scrolled message list
│   │   │   ├── TranscriptItem.vue    # Single message
│   │   │   ├── ModelCallCard.vue     # Model I/O detail card
│   │   │   ├── Composer.vue     # Input area
│   │   │   └── PanelTabs.vue    # Graph / Files / Diff tabs
│   │   │
│   │   ├── graph/
│   │   │   └── GraphPanel.vue   # Cytoscape.js wrapper
│   │   │
│   │   ├── history/
│   │   │   ├── RunList.vue      # History list (sidebar)
│   │   │   └── RunItem.vue      # Single history entry
│   │   │
│   │   ├── checkpoint/
│   │   │   ├── CheckpointTree.vue    # Branch tree (sidebar)
│   │   │   └── CheckpointNode.vue    # Single checkpoint
│   │   │
│   │   ├── config/
│   │   │   ├── SettingsView.vue      # (route: /settings)
│   │   │   ├── ModelConfig.vue       # Model tier config
│   │   │   ├── PolicyConfig.vue      # Tool policy config
│   │   │   └── ToolsConfig.vue       # Tool registry config
│   │   │
│   │   ├── files/
│   │   │   ├── FilesPanel.vue        # Changed files list
│   │   │   └── DiffPanel.vue         # Code diff viewer
│   │   │
│   │   ├── skills/
│   │   │   └── SkillsView.vue        # (route: /skills)
│   │   │
│   │   └── shared/
│   │       ├── TopBar.vue            # DetailModeToggle + StatusPill
│   │       ├── DetailModeToggle.vue  # Switch for model I/O visibility
│   │       ├── StatusPill.vue        # Run status + token count
│   │       └── ToastStack.vue        # Global notifications
│   │
│   └── styles/
│       └── main.css                 # Global styles + CSS variables
│
└── public/
    └── vendor/
        └── cytoscape.min.js         # Copied from old webui
```

### 3.2 Key Technical Decisions

**Transcript performance:**
- Use virtual scrolling library (vue-virtual-scroller or custom IntersectionObserver)
  when message count exceeds 200
- Each message is a Vue component with `:key="message.id"` — new messages push to
  the reactive array, Vue only renders the new DOM node
- No `innerHTML` anywhere. Each message renders via `<template>` + `{{ }}`

**Cytoscape integration:**
- `GraphPanel.vue` creates Cytoscape instance in `onMounted`
- Watches `props.nodes` and `props.edges` reactively
- On change: calls `cy.add()` for new elements, `cy.remove()` for removed ones
  (incremental, not full rebuild)
- No re-creation of the Cytoscape instance on re-render

**WebSocket reconnection:**
- `useRunSocket` composable implements exponential backoff (1s → 2s → 4s → max 30s)
- On reconnect, requests full state snapshot via REST as fallback sync

**Detail mode toggle:**
- Local state in `useRunSocket` composable
- Toggle sends `set_detail_mode` to server to skip serialization of verbose events
- `ModelCallCard.vue` components only render when detail mode is on

### 3.3 Build Integration

- `webui/` is an independent npm project
- `npm run dev` → Vite dev server with HMR (port 5173)
- `npm run build` → produces `webui/dist/`
- `cargo run --bin serve` serves `webui/dist/` via axum `ServeDir`
- `webui/dist/` is in `.gitignore`
- Release script: `npm run build && cargo build --release`

---

## 4. Checkpoint & Branch System

### 4.1 Data Model

```rust
// src/web/checkpoint.rs

pub struct Checkpoint {
    pub index: usize,
    pub round: usize,
    pub phase: CheckpointPhase,
    pub graph_snapshot: Graph,           // clone of graph at this point
    pub transcript: Vec<Message>,        // clone of conversation.messages
    pub created_at: Instant,
}

pub enum CheckpointPhase { Graph, Task, Review }

pub struct CheckpointStore {
    checkpoints: Vec<Checkpoint>,
    branches: HashMap<usize, Vec<String>>,  // checkpoint_index → [child_run_ids]
}
```

### 4.2 Checkpoint Lifecycle

1. **Creation:** After each `step()` call, `drive_run` pushes a checkpoint.
   Creation is non-blocking — `graph.clone()` + `messages.clone()` are
   in-memory operations.

2. **Listing:** `GET /api/runs/:id/checkpoints` returns lightweight metadata
   (index, round, phase, node_count) for the sidebar tree.

3. **Retrieval:** `GET /api/runs/:id/checkpoints/:idx` returns the full
   snapshot for replay.

4. **Branching:** `POST /api/runs/:id/branch { from_checkpoint: 17 }`:
   - Clone the checkpoint's (graph, transcript)
   - Create a new run with `initial_graph` and `initial_transcript`
   - Record the branch relationship in `CheckpointStore.branches`
   - Return the new run ID

### 4.3 Frontend Interaction

**In-transcript branching (Feature A):**
- Each `TranscriptItem` has a hover state showing a "Fork from here" button
- Clicking it:
  1. Posts `POST /api/runs/:id/branch { from_checkpoint: <matching index> }`
  2. Receives new run ID
  3. Navigates to the new run's view with fresh WebSocket connection

**History browsing (Feature B):**
- Sidebar `RunList` shows all past runs
- Clicking a run loads it in read-only mode:
  - Full transcript visible
  - Final graph rendered
  - No live WebSocket (run is complete)
- "Continue" button creates a branch from the last checkpoint

---

## 5. New SSE/WS Event Types

### 5.1 model_call

Emitted for every model invocation (Proposer, Verifier, Reviewer, L1Enricher,
CascadeBacktracker). Only transmitted when `detail_mode` is enabled.

```json
{
  "type": "model_call",
  "data": {
    "component": "Proposer",
    "model_name": "deepseek-v4-pro",
    "tier": "deep",
    "request": {
      "messages": [...],
      "temperature": 0.1,
      "max_tokens": 4096,
      "tool_count": 0
    },
    "response": {
      "content": "...",
      "finish_reason": "stop",
      "usage": { "prompt_tokens": 2340, "completion_tokens": 512 }
    },
    "duration_ms": 3456
  }
}
```

### 5.2 cascade_step

Emitted for each predecessor verification during cascade backtracking.

```json
{
  "type": "cascade_step",
  "data": {
    "changed_node": "C",
    "predecessor": "B",
    "depth": 1,
    "verdict": "preserved",
    "rationale": "B's output type matches C's new input requirement",
    "duration_ms": 1234
  }
}
```

### 5.3 checkpoint

Emitted when a checkpoint is created (lightweight notification, not full data).

```json
{
  "type": "checkpoint",
  "data": {
    "index": 17,
    "round": 5,
    "phase": "task",
    "node_count": 12,
    "edge_count": 15
  }
}
```

### 5.4 stream_chunk / stream_end

Emitted for every model invocation when streaming is active. Each token
(likely more than one) produces a `stream_chunk`; each model call ends with
a `stream_end`. The `component` field identifies the model tier ("fast" or
"deep").

```json
{
  "type": "stream_chunk",
  "data": {
    "component": "fast",
    "content": "partial token text",
    "reasoning_content": null,
    "finish_reason": null
  }
}
```

```json
{
  "type": "stream_end",
  "data": {
    "component": "fast",
    "finish_reason": "stop",
    "prompt_tokens": 2340,
    "completion_tokens": 512
  }
}
```

Streaming is transparent at the `Model` trait level — `ModelWithEvents`
([`src/model/streaming.rs`](../../src/model/streaming.rs)) wraps any model
and routes `complete()` through `complete_stream()`, forwarding deltas to
the broadcast channel. See §13 of `ARCHITECTURE.md` for the full
architecture.

---

## 6. Config API

### 6.1 Data Model

```rust
// src/web/state.rs
pub struct EngineConfig {
    pub model: ModelTierConfig,
    pub policy: ToolPolicyConfig,
    pub tools: ToolRegistryConfig,
    pub loop_config: LoopTuningConfig,
}

pub struct ModelTierConfig {
    pub base_url: String,
    pub api_key: String,           // masked in GET response
    pub fast_model: String,
    pub deep_model: String,
    pub default_model: Option<String>,
}

pub struct ToolPolicyConfig {
    pub deny_patterns: Vec<String>,     // DangerousCommandDeny patterns
    pub implicit_cwd_verbs: Vec<String>, // build tools to allow
    pub max_concurrent_subagents: usize,
}

pub struct LoopTuningConfig {
    pub max_rounds: usize,            // default 300
    pub max_repair_rounds: usize,     // default 4
    pub cascade_backtrack: bool,      // default true
}
```

### 6.2 Endpoints

```
GET /api/config → EngineConfig (api_key masked)
POST /api/config → { "model": { "fast_model": "new-model" } } → partial update
```

Config changes take effect on the NEXT run creation. Running runs are not
affected (to avoid mid-run instability).

---

## 7. Data Flow: Key Scenarios

### 7.1 Normal run (no failures)

```
User types task → POST /api/runs → run created, WS upgrade
  → driver spawns → GraphLoop steps:
    step() → Proposer emits propose_patch → graph updated
           → WS: transcascript, graph_snapshot, checkpoint
    step() → Proposer emits ready_for_verify → Verifier runs
           → WS: model_call (if detail_mode), checkpoint
    step() → passes → Task phase → Dispatcher → sub-agent execution
           → WS: transcascript (sub-agent results), checkpoint
    step() → Review → WS: status(done), done
```

### 7.2 Sub-agent failure with cascade backtrack

```
Task phase: sub-agent at node C reports report_graph_error
  → step_task_stub detects graph errors
  → handle_task_phase_graph_error():
    1. Feed failure evidence to Proposer
    2. Proposer re-plans: C → C', downstream nodes adjusted
    3. Patch applied to graph
    4. CascadeBacktracker.backtrack_from(C'):
       - Verify B1 (predecessor): model_call emitted
       - B1 preserved: cascade_step emitted
       - Verify B2 (predecessor): model_call emitted
       - B2 needs repair: cascade_step emitted
       - Recurse to B2's predecessors...
       - Reaches anchor A: stop
    5. Re-execute nodes that need fresh outputs
    6. Continue forward execution from deepest repaired node
  → All cascade_step and model_call events visible in UI when detail_mode=on
```

### 7.3 Branch creation

```
User hovers over message at transcript line 34
  → Clicks "Fork from here"
  → POST /api/runs/:id/branch { from_checkpoint: 34 }
  → Server:
    - Looks up checkpoint[34]
    - Creates new run with initial_graph=checkpoint.graph,
      initial_transcript=checkpoint.transcript
    - Returns { id: "new-run-uuid" }
  → Frontend:
    - Navigates to new run
    - Opens WebSocket to /ws/runs/new-run-uuid
    - User continues from that point with new task input
```

### 7.4 Budget exhaustion

```
Round 300 reached
  → step() returns Paused with:
    question = "Budget exhausted. Here's what succeeded and what failed..."
    rationale = graph summary + failure list
  → WS: checkpoint (final state), status(paused)
  → User sees: which nodes succeeded, which failed, last error evidence
  → User can: provide guidance, modify graph, or terminate
```

---

## 8. Migration from v1

### 8.1 Files to Create
- `src/agent/cascade.rs` — CascadeBacktracker
- `src/web/ws.rs` — WebSocket handler
- `src/web/checkpoint.rs` — CheckpointStore
- `src/web/config_api.rs` — Config CRUD
- `webui/` — complete Vue 3 project

### 8.2 Files to Modify
- `src/graph/mod.rs` — immutable flag, predecessors_of, set_anchor
- `src/agent/graph_loop.rs` — auto-replan, 300 rounds, cascade integration
- `src/agent/mod.rs` — re-export cascade module
- `src/web/mod.rs` — router add WS + config endpoints
- `src/web/state.rs` — add EngineConfig
- `src/web/events.rs` — add model_call, cascade_step, checkpoint events
- `src/web/run_session.rs` — add CheckpointStore
- `src/web/api_runs.rs` — branching endpoint, WS upgrade

### 8.3 Files to Remove
- `webui/app.js` — replaced by Vue build output
- `webui/app.css` — replaced by Vue styles
- `webui/index.html` — replaced by Vite template

### 8.4 Things Preserved
- All 310 existing tests continue to pass
- `agent_a` CLI binary (unchanged)
- `graph_harness` binary (unchanged)
- Skill storage system (unchanged)
- Cytoscape.js vendor library (copied to new webui/public/vendor/)

---

## 9. Self-Review Notes

- **Placeholders:** None. All components have concrete type signatures and API shapes.
- **Consistency:** REST + WS event types consistent across Rust structs and JSON
  schemas. Cascade backtracking flow aligned with design doc (§4).
- **Scope:** Single coherent deliverable — v2 engine + new comm layer + new frontend.
  No decomposition needed.
- **Ambiguity:** Detail mode filtering happens server-side (skip serialization) AND
  client-side (ModelCallCard not rendered). This avoids the "double-check" race
  condition where events in flight during toggle could be lost.
