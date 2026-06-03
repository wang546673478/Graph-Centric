# Web Gateway — Browser-Based Skill Management + Agent Runtime

**Date:** 2026-06-03
**Status:** Approved (pending spec review)
**Scope:** New binary `bin/serve.rs` (axum HTTP server + SvelteKit frontend) that wraps the existing `GraphLoop` + `SubAgent` + `SkillStorage` for browser-based interaction.

## 1. Context

The harness today is CLI-only: `bin/agent_a` reads a task, loops with user input via stdin, and exits. There is no way to:

- See the relationship graph grow in real time as the agent reasons
- Browse the captured skills (`~/.local/share/graph-centric/skills/`, `<project>/skills/`)
- See run history with verdict + duration
- View file diffs the agent made
- Cancel a runaway run

The user has decided to build an OpenClaw-style gateway: a web server that embeds the existing agent loop and exposes it via REST + SSE. v1 is single-process (gateway + agent in one binary), single-user (localhost-only, no auth). The agent's existing infrastructure — `GraphLoop`, `SubAgent`, `SkillStorage` — is reused without modification.

## 2. Goal

`cargo run --bin serve` starts a web server on `localhost:8080`. The user opens a browser and:

- **Drives the agent** via a chat box (replaces stdin)
- **Watches the graph grow** in real time via SSE-pushed events
- **Browses captured skills** in a library page (list + search + view full graph + promote local → repo)
- **Reviews run history** with transcript, final graph, review verdict, captured skill
- **Sees file diffs** in a dedicated tab
- **Cancels runaway runs** with a Stop button

The agent retains its existing safety: `DangerousCommandDeny` policy + auto-derived `ScopeGuard` from `task.involved_nodes`. The web UI is a thinner HTTP surface over the same logic — no new agent logic.

## 3. Non-goals (YAGNI)

- ❌ No auth / multi-user / multi-tenant. localhost-only, single user.
- ❌ No CLI for skill management (the web UI replaces the planned `graph-harness skill list/promote/...` CLI).
- ❌ No real-time graph diff (v1 sends full graph snapshot per event; v2 may add deltas).
- ❌ No skill editor (delete + re-run is enough).
- ❌ No worker/gateway split (v1 single binary; v2 may split per OpenClaw style).
- ❌ No CORS config (same-origin only; v2 if deployed).
- ❌ No persistence of run history beyond the current process (in-memory `Arc<RwLock<HashMap<RunId, RunSession>>>`; v2 may add disk).
- ❌ No rate limiting / DoS protection (localhost).
- ❌ No HTTPS (localhost).
- ❌ No file editing in browser (read-only diff; edit via local `git`/editor).
- ❌ No skill marketplace / sharing.
- ❌ No observability (metrics, traces).

## 4. Design

### 4.1 Architecture

Single binary `bin/serve.rs`. Inside:

```
┌────────────────────────────────────────────────────────┐
│  axum Router on 0.0.0.0:8080                           │
│                                                        │
│  ┌───── HTTP handlers (axum extractors) ────┐          │
│  │  /api/runs/*        (create, list, get,    │          │
│  │  /api/skills/*      (list, get, promote,   │          │
│  │  /api/files/*       (list, diff)            │          │
│  │  /api/runs/{id}/events  (SSE)               │          │
│  │  /api/runs/{id}/answer  (resume Paused)    │          │
│  │  /api/runs/{id}/repair  (resume GraphInv)   │          │
│  │  /api/runs/{id}/cancel  (Stop)              │          │
│  │  /                  (static SvelteKit)     │          │
│  └────────────┬─────────────────────────────────┘          │
│               │                                         │
│  ┌────────────▼─────────────────────────────────┐          │
│  │  WebState (Arc<RwLock<...>>)               │          │
│  │  - runs: HashMap<RunId, RunSession>       │          │
│  │  - skill_storage: Arc<dyn SkillStorage>   │          │
│  │  - event_broadcasters: per-RunId channel  │          │
│  └────────────┬─────────────────────────────────┘          │
│               │                                         │
│  ┌────────────▼─────────────────────────────────┐          │
│  │  Existing code (reused, not modified)     │          │
│  │  - GraphLoop + GraphProposer               │          │
│  │  - SubAgent (with DangerousCommandDeny +  │          │
│  │    auto-derived ScopeGuard)                │          │
│  │  - SkillStorage (Local + Repo + Composite)│          │
│  │  - Skill capture flow (fire-and-forget)    │          │
│  └────────────────────────────────────────────┘          │
└────────────────────────────────────────────────────────┘
```

The web module is a thin layer. It does not duplicate agent logic; it exposes it.

### 4.2 Tech stack

| Layer | Choice | Rationale |
|---|---|---|
| Backend HTTP | `axum 0.7` | tokio-native, SSE first-class, idiomatic for async Rust |
| Frontend framework | `SvelteKit` + `TypeScript` | Lightweight, compile-time optimized, good component ecosystem |
| Styling | `Tailwind CSS` + `shadcn-svelte` | Modern look without bespoke CSS |
| Graph viz | `Cytoscape.js` | Mature, fast up to 5000 nodes, cose-bilkent layout |
| Diff view (v1) | `diff2html` + `Prism.js` | Render unified diff as HTML with syntax highlight |
| Editor (v2) | `Monaco Editor` | Standard for in-browser code editing |
| Real-time | `Server-Sent Events` (SSE) | One-way server → client, native browser `EventSource` |
| Process model | Single `bin/serve.rs` | Simpler than worker/gateway split |

### 4.3 HTTP API surface

| Method | Path | Body | Returns | Notes |
|---|---|---|---|---|
| `GET` | `/` | — | HTML | SvelteKit SPA |
| `GET` | `/api/health` | — | `{status: "ok"}` | Liveness |
| `GET` | `/api/runs` | — | `[{id, task, status, verdict, started_at, duration_ms}]` | Run history (in-memory) |
| `POST` | `/api/runs` | `{task: string}` | `{id}` | Start new run; returns RunId immediately |
| `GET` | `/api/runs/{id}` | — | `{id, task, status, final_result, captured_skill?}` | Run metadata |
| `DELETE` | `/api/runs/{id}` | — | `{cancelled: true}` | Stop a running agent (idempotent) |
| `GET` | `/api/runs/{id}/events` | — | `text/event-stream` | SSE stream of run events |
| `POST` | `/api/runs/{id}/answer` | `{answer: string}` | `{accepted: true}` | Resume a `Paused` run |
| `POST` | `/api/runs/{id}/repair` | `{graph: Graph}` | `{accepted: true}` | Resume a `GraphInvalid` run |
| `GET` | `/api/skills` | — | `[{slug, trigger, created_at, source}]` | List all skills (local + repo) |
| `GET` | `/api/skills/{slug}` | — | `{slug, trigger, graph, meta}` | Skill detail |
| `POST` | `/api/skills/{slug}/promote` | — | `{promoted: true}` | Copy local → repo |
| `DELETE` | `/api/skills/{slug}` | — | `{deleted: true}` | Delete skill (local or repo) |
| `GET` | `/api/files/changed` | `?since=...` | `[{path, change_type}]` | List files changed in a window (uses `git diff --name-only`) |
| `GET` | `/api/files/diff` | `?path=...` | `string` (unified diff) | Diff of one file vs `git HEAD` |

Error responses: `{error: {code, message}}` with HTTP status 4xx/5xx. Mapped from `HarnessError` and `SkillError`.

### 4.4 SSE event types

The server emits JSON-encoded events with a `type` discriminator. Each `RunSession` has its own `tokio::sync::broadcast::Sender<Event>`; the SSE handler subscribes and forwards.

```rust
#[derive(Serialize)]
#[serde(tag = "type", content = "data")]
pub enum RunEvent {
    Transcript { role: String, content: String },
    GraphSnapshot { nodes: Vec<NodeDto>, edges: Vec<EdgeDto> },
    LoopState { kind: String, payload: serde_json::Value },
    Review { verdict: String, root_cause: Option<String> },
    SkillCaptured { slug: String, trigger: String },
    Done { final_result: serde_json::Value },
    Error { message: String },
}
```

Wire format (SSE):
```
event: transcript
data: {"role":"assistant","content":"..."}

event: graph
data: {"nodes":[...],"edges":[...]}

event: done
data: {"final_result":{...}}
```

The `event:` field is the discriminator. Client uses `EventSource.addEventListener("type", ...)`.

### 4.5 Frontend pages

Three primary routes, single SPA:

```
/                → main run view (chat + graph + tabs)
/skills         → skill library
/runs           → run history
/settings       → model config + .env status (minimal)
```

#### Main run view (the working view)

```
┌─ Graph-Centric ────────── [Runs] [Skills] [Settings] ─────┐
│ Active: abc123 "review proposer.rs"  ⏱️ 8.2s  [Stop]   │
│ ┌─── Chat (left, 50%) ─┐  ┌─── Tabs (right, 50%) ──────┐ │
│ │                       │  │ [Graph] [Files] [Diff]   │ │
│ │ [proposer] 12 L0     │  │                          │ │
│ │ nodes proposed        │  │   Cytoscape canvas       │ │
│ │                       │  │   (force-directed)        │ │
│ │ [ask user] What       │  │                          │ │
│ │ level of review?      │  │   Nodes: 14  Edges: 23   │ │
│ │ [▸ 3 buttons]         │  │                          │ │
│ │                       │  │   click node → L1 panel  │ │
│ │ [user] Full review    │  │                          │ │
│ │                       │  │                          │ │
│ │ [Done] ✅ 12.3s       │  │                          │ │
│ │ Skill captured: ...   │  │                          │ │
│ ├───────────────────────┤  │                          │ │
│ │ [Type a task...]  [→] │  │                          │ │
│ └───────────────────────┘  └──────────────────────────┘ │
└────────────────────────────────────────────────────────────┘
```

Three tabs in the right pane: **Graph** (default), **Files** (list of recently changed), **Diff** (unified diff with syntax highlight).

#### Skill library

List of all skills (local + repo), each with trigger one-liner. Click → detail modal showing full graph + meta + [Promote to repo] / [Delete].

#### Run history

Table of past runs: id, task, status (✅/❌), verdict, duration, captured-skill slug. Click → transcript + graph + review + file diffs.

### 4.6 File structure

**Rust side (modified/new):**
```
Cargo.toml                            ← add axum, tower, tower-http, tokio-stream
src/
├── lib.rs                            ← no change
├── web/                              ← NEW MODULE
│   ├── mod.rs                        axum Router + state + handler registration
│   ├── state.rs                      WebState: runs map + skill storage + config
│   ├── events.rs                     RunEvent enum + broadcast helpers
│   ├── api_runs.rs                   /api/runs/* handlers
│   ├── api_skills.rs                 /api/skills/* handlers
│   ├── api_files.rs                  /api/files/* handlers (git-based diff)
│   ├── run_session.rs                per-run state machine + cancel token
│   └── errors.rs                     ApiError + IntoResponse mapping
└── bin/
    └── serve.rs                      NEW BINARY: tokio main → axum serve
```

**Frontend (new directory):**
```
webui/                                ← NEW SvelteKit project
├── package.json
├── svelte.config.js
├── vite.config.ts
├── tailwind.config.cjs
├── tsconfig.json
├── postcss.config.cjs
├── src/
│   ├── app.html
│   ├── app.css
│   ├── routes/
│   │   ├── +layout.svelte            shared header + nav
│   │   ├── +page.svelte              main run view
│   │   ├── skills/+page.svelte
│   │   ├── runs/+page.svelte
│   │   └── settings/+page.svelte
│   └── lib/
│       ├── api.ts                    typed fetch wrapper (ApiError class)
│       ├── sse.ts                    EventSource client with reconnect
│       ├── graph.ts                  Cytoscape wrapper (init, update, click)
│       ├── diff.ts                   diff2html wrapper
│       └── types.ts                  shared types (RunEvent, Skill, etc.)
├── static/                           (placeholder; not used in v1)
└── .gitignore                        node_modules, .svelte-kit, build/
```

### 4.7 Per-run state machine (`RunSession`)

```rust
pub struct RunSession {
    pub id: RunId,
    pub task: String,
    pub started_at: Instant,
    pub status: RunStatus,
    pub event_tx: broadcast::Sender<RunEvent>,
    pub cancel: CancellationToken,         // signals SubAgent to stop
    pub pending_question: Option<String>,   // for Paused
    pub last_graph: Arc<RwLock<Graph>>,
    pub last_review: Arc<RwLock<Option<ReviewResult>>>,
    pub captured_skill: Arc<RwLock<Option<SkillRef>>>,
}

pub enum RunStatus {
    Running,
    Paused,
    GraphInvalid,
    Done { result: FinalResult },
    Error(String),
    Cancelled,
}
```

The run is driven by a `tokio::spawn`'d task that:
1. Constructs `GraphLoop` (with `proposer`, `verifier`, `repairer`, `enricher`, `decomposer`, `subagent`, `reviewer`, `validator` — same as `bin/agent_a`).
2. Loops on `gl.step()`:
   - Map each `LoopState` to one or more `RunEvent`s and broadcast them
   - On `Paused { question }`, set `pending_question`, wait on a `Notify`
   - On `GraphInvalid { errors, snapshot }`, wait on a `Notify` for the repaired graph
   - On `Done(_)`, capture fire-and-forget, store captured_skill
   - On any `step()`, check `cancel.is_cancelled()` and bail early if set
3. Final state is recorded in `RunStatus`.

### 4.8 Cancellation flow

The `DELETE /api/runs/{id}` handler:
1. Looks up the run by id
2. If status is `Running` or `Paused`, calls `cancel.cancel()`
3. The run loop checks `cancel.is_cancelled()` on each iteration and exits early
4. Final status becomes `Cancelled` and a `Done { cancelled }` event is emitted

`CancellationToken` is from `tokio_util::sync::CancellationToken`. The model call itself isn't killed (tokio can't easily kill an in-flight HTTP request), but the next `step()` call bails.

### 4.9 Answer / repair flow

The `POST /api/runs/{id}/answer` handler:
1. Looks up the run
2. If `status == Paused`, stores the answer in `pending_answer` and notifies the wait
3. The run loop wakes up, calls `gl.resume(answer)`, and continues

Same pattern for `/api/runs/{id}/repair` (stores the repaired graph, calls `gl.resume_with_repaired_graph(repaired)`).

### 4.10 Skill storage wiring

The web state's `WebState.skill_storage` is the same `CompositeSkillStorage` already used in `bin/agent_a`:

```rust
let local_root = LocalSkillStorage::default_install()
    .map(|s| s.root)
    .unwrap_or_else(|| std::env::temp_dir().join("graph-centric-skills-fallback"));
let repo_root = std::env::current_dir()
    .map(|p| p.join("skills"))
    .unwrap_or_else(|_| PathBuf::from("skills"));

let skill_storage: Arc<dyn SkillStorage> = Arc::new(CompositeSkillStorage::new(
    LocalSkillStorage::new(local_root),
    RepoSkillStorage::new(repo_root),
));
```

The Proposer in the run session receives this via `with_skills(...)` (already implemented in Task 9 of the prior plan).

### 4.11 File diff via git

The `/api/files/changed` and `/api/files/diff` endpoints shell out to `git`:

```rust
async fn files_changed(since: DateTime<Utc>) -> Vec<String> {
    let output = Command::new("git")
        .args(["diff", "--name-only", "--since", since.to_rfc3339()])
        .output().await.unwrap();
    String::from_utf8_lossy(&output.stdout).lines().map(String::from).collect()
}

async fn file_diff(path: &str) -> String {
    let output = Command::new("git")
        .args(["diff", "HEAD", "--", path])
        .output().await.unwrap();
    String::from_utf8_lossy(&output.stdout).to_string()
}
```

(Returns empty / friendly error if not in a git repo.)

### 4.12 Error mapping

```rust
// src/web/errors.rs
pub enum ApiError {
    NotFound(String),
    BadRequest(String),
    Internal(String),
}

impl From<HarnessError> for ApiError { /* map to Internal */ }
impl From<SkillError> for ApiError {
    fn from(e: SkillError) -> Self {
        match e {
            SkillError::NotFound(s) => Self::NotFound(s),
            SkillError::InvalidSlug(s) => Self::BadRequest(s),
            _ => Self::Internal(e.to_string()),
        }
    }
}

impl IntoResponse for ApiError { /* status code + JSON body */ }
```

### 4.13 Frontend SSE client (key code)

```ts
// webui/src/lib/sse.ts
export class RunEventSource {
    private es: EventSource;
    private handlers = new Map<string, (data: any) => void>();

    constructor(runId: string) {
        this.es = new EventSource(`/api/runs/${runId}/events`);
        this.es.addEventListener('transcript', (e: MessageEvent) => {
            this.emit('transcript', JSON.parse(e.data));
        });
        this.es.addEventListener('graph', (e: MessageEvent) => {
            this.emit('graph', JSON.parse(e.data));
        });
        // ... etc for each event type
        this.es.onerror = () => {
            this.es.close();
            setTimeout(() => this.reconnect(), 3000);
        };
    }
    on(type: string, handler: (data: any) => void) {
        this.handlers.set(type, handler);
    }
    private emit(type: string, data: any) {
        this.handlers.get(type)?.(data);
    }
    close() { this.es.close(); }
}
```

## 5. Files

**New (Rust):**
- `Cargo.toml` — add `axum = "0.7"`, `tower = "0.5"`, `tower-http = "0.5"`, `tokio-util = "0.7"`, `tokio-stream = "0.1"`
- `src/web/mod.rs` (~80 lines)
- `src/web/state.rs` (~100 lines)
- `src/web/events.rs` (~80 lines)
- `src/web/run_session.rs` (~200 lines)
- `src/web/api_runs.rs` (~200 lines)
- `src/web/api_skills.rs` (~100 lines)
- `src/web/api_files.rs` (~100 lines)
- `src/web/errors.rs` (~60 lines)
- `src/bin/serve.rs` (~80 lines)

**New (Frontend):**
- `webui/package.json`, `webui/svelte.config.js`, `webui/vite.config.ts`, `webui/tsconfig.json`, `webui/tailwind.config.cjs`, `webui/postcss.config.cjs`, `webui/.gitignore`
- `webui/src/app.html`, `webui/src/app.css`
- `webui/src/routes/+layout.svelte`, `+page.svelte`, `skills/+page.svelte`, `runs/+page.svelte`, `settings/+page.svelte`
- `webui/src/lib/{api,sse,graph,diff,types}.ts`

**Modified:**
- `Cargo.toml` (new deps as above)
- `src/lib.rs` (add `pub mod web;`)

**No changes to:** `src/agent/`, `src/skills/`, `src/tools/`, `src/graph/`, `src/model/`, `src/bin/agent_a.rs`, `src/bin/demo.rs`, `src/bin/probe_model.rs`.

## 6. Tests

### 6.1 Unit: events
- `run_event_serializes_with_type_discriminator` — verify JSON shape
- `run_event_deserializes_from_sse_data_line` — round-trip

### 6.2 Unit: state
- `webstate_starts_empty_runs_map` — fresh state has 0 runs
- `webstate_insert_and_get_run` — basic CRUD on runs map

### 6.3 Unit: run_session
- `run_session_starts_in_running_status` — new session has `Running`
- `run_session_cancellation_sets_status` — cancel token → status `Cancelled`
- `run_session_emits_initial_graph_snapshot` — first event is `graph`

### 6.4 Unit: errors
- `api_error_from_skill_not_found` — 404
- `api_error_from_skill_invalid_slug` — 400
- `api_error_into_response_status_code` — verify each variant's status

### 6.5 Integration: API endpoints
- `api_health_returns_ok`
- `api_runs_post_creates_run_and_returns_id`
- `api_runs_get_returns_run_metadata`
- `api_runs_delete_cancels_running_run`
- `api_runs_events_returns_sse_with_correct_content_type`
- `api_runs_answer_resumes_paused_run`
- `api_runs_repair_resumes_graph_invalid_run`
- `api_skills_list_returns_all`
- `api_skills_get_returns_skill_detail`
- `api_skills_promote_copies_local_to_repo`
- `api_skills_delete_removes_skill`
- `api_files_diff_returns_git_diff_output`

(Use `axum::Router` + `tower::ServiceExt::oneshot` to test handlers in-process — no actual network binding needed.)

### 6.6 Integration: end-to-end
- `serve_starts_runs_responds_to_health` — boots axum on a random port, GET /api/health returns 200
- `full_run_lifecycle_via_http` — POST /api/runs, GET events via SSE, POST /api/runs/{id}/answer when Paused, wait for Done, GET /api/runs/{id} shows Done status

### 6.7 Manual smoke (post-implementation)
- `cargo run --bin serve` opens a browser; chat box drives a real agent; graph updates in real time; skills are captured and listed
- File diff tab shows actual diffs

## 7. Acceptance criteria

- [ ] `cargo run --bin serve` starts an HTTP server on `0.0.0.0:8080` (or `$WEB_PORT`)
- [ ] `GET /api/health` returns 200 with `{"status": "ok"}`
- [ ] `POST /api/runs` starts a run and returns a RunId
- [ ] `GET /api/runs/{id}/events` returns `text/event-stream` with events for the run
- [ ] Events include: `transcript`, `graph`, `loop_state`, `review`, `skill_captured`, `done`, `error`
- [ ] The graph event shows the L0+L1 growing in real time
- [ ] `DELETE /api/runs/{id}` cancels a running agent
- [ ] `POST /api/runs/{id}/answer` resumes a `Paused` run
- [ ] `POST /api/runs/{id}/repair` resumes a `GraphInvalid` run
- [ ] `GET /api/skills` lists all skills (local + repo)
- [ ] `POST /api/skills/{slug}/promote` copies local skill to `skills/`
- [ ] `GET /api/files/diff?path=...` returns a unified diff (empty if no git)
- [ ] Frontend loads and shows: chat box, graph view, file diff tab
- [ ] Stop button in UI calls `DELETE /api/runs/{id}` and shows "Cancelled" in the UI
- [ ] Skill library page lists all skills with their trigger one-liners
- [ ] `cargo test -p graph_harness` shows all existing tests still pass
- [ ] New integration tests pass (target: ~10 new tests)
- [ ] `cargo check -p graph_harness` clean
- [ ] No CORS headers needed (same origin in v1)
- [ ] `webui/node_modules/` and `webui/.svelte-kit/` in `.gitignore`

## 8. Out-of-scope (v2+)

- **v2:** Worker/gateway split (OpenClaw-style) — `bin/serve` as a thin HTTP proxy, `bin/agent_a` as the worker, IPC via Unix socket or HTTP
- **v2:** Persistence of run history (disk-based store)
- **v2:** `graph_delta` SSE event (incremental graph updates, not full snapshots)
- **v2:** Monaco Editor for in-browser file editing
- **v2:** Skill editor (rename, edit trigger, edit L1 descriptions)
- **v2:** Graph diff viewer (compare current graph to a saved skill's graph)
- **v2:** Multi-user (auth + per-user run isolation)
- **v2:** Embedding-based skill retrieval in the Proposer
- **v2:** Deployment guide (HTTPS, systemd, docker)
- **v3:** Skill marketplace / sharing across installations
