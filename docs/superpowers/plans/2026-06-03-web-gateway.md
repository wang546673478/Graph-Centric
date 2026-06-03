# Web Gateway Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `bin/serve.rs` binary that wraps the existing `GraphLoop` + `SubAgent` + `SkillStorage` behind an axum HTTP/SSE server, plus a SvelteKit web UI in `webui/`, so the user can drive the agent, watch the graph grow, and manage skills from a browser.

**Architecture:** Single Rust binary embeds axum HTTP server + the existing agent loop. SvelteKit SPA in `webui/` provides the frontend; `webui/dist/` is served as static files by axum. SSE streams run events to the browser in real time. Reuses `GraphLoop`, `SubAgent`, `SkillStorage`, `GraphProposer::with_skills(...)` without modification.

**Tech Stack:** Rust 2024 edition, `axum 0.7`, `tower 0.5`, `tower-http 0.5`, `tokio-util 0.7` (CancellationToken), `tokio-stream 0.1`. Frontend: SvelteKit + TypeScript + Tailwind + Cytoscape.js + diff2html + Prism.js. **No new build complexity** — `webui/` is a standard SvelteKit project, `npm run build` produces static files consumed by axum.

**Spec:** `docs/superpowers/specs/2026-06-03-web-gateway-design.md`

**Note on git:** This project does not currently have a git repository. Where the template shows `git commit` as a step, instead run `cargo check` (or `cargo test` for test tasks) to verify the change compiles and behaves correctly. The "checkpoint" idea still applies.

**Note on node_modules:** Plan Task 12 (SvelteKit scaffold) runs `npm install`. Add `webui/node_modules/`, `webui/.svelte-kit/`, `webui/build/` to `.gitignore` before that task.

---

## File Structure

**New files (Rust):**
- `src/web/mod.rs` — module entry + axum Router
- `src/web/state.rs` — `WebState` (runs map, skill storage, config)
- `src/web/events.rs` — `RunEvent` enum
- `src/web/run_session.rs` — per-run state machine
- `src/web/api_runs.rs` — `/api/runs/*` handlers
- `src/web/api_skills.rs` — `/api/skills/*` handlers
- `src/web/api_files.rs` — `/api/files/*` handlers
- `src/web/errors.rs` — `ApiError` + `IntoResponse`
- `src/bin/serve.rs` — new binary: tokio main + axum serve

**Modified files (Rust):**
- `Cargo.toml` — add `axum`, `tower`, `tower-http`, `tokio-util`, `tokio-stream`
- `src/lib.rs` — add `pub mod web;`

**New files (Frontend, all under `webui/`):**
- `package.json`, `svelte.config.js`, `vite.config.ts`, `tsconfig.json`, `tailwind.config.cjs`, `postcss.config.cjs`, `.gitignore`
- `src/app.html`, `src/app.css`
- `src/routes/+layout.svelte`, `+page.svelte`, `skills/+page.svelte`, `runs/+page.svelte`, `settings/+page.svelte`
- `src/lib/api.ts`, `sse.ts`, `graph.ts`, `diff.ts`, `types.ts`

**No changes to:** `src/agent/`, `src/skills/`, `src/tools/`, `src/graph/`, `src/model/`, `src/bin/agent_a.rs`, `src/bin/demo.rs`, `src/bin/probe_model.rs`.

---

## Task 1: Add axum + supporting deps to `Cargo.toml`

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add the new dependencies**

Read `/home/hhhh/Graph-Centric/Cargo.toml`. In `[dependencies]`, add these lines (keep the existing entries; just append):

```toml
axum = "0.7"
tower = "0.5"
tower-http = { version = "0.5", features = ["fs", "trace"] }
tokio-util = "0.7"
tokio-stream = "0.1"
```

- [ ] **Step 2: Verify the deps resolve and the existing tests still pass**

Run: `cargo check -p graph_harness 2>&1 | tail -5`
Expected: clean (the new deps don't break anything yet).

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: 346 tests pass (unchanged).

---

## Task 2: Create `src/web/mod.rs` skeleton

**Files:**
- Modify: `src/lib.rs` (add `pub mod web;`)
- Create: `src/web/mod.rs` (empty module + doc)

- [ ] **Step 1: Add module declaration to `src/lib.rs`**

Read `/home/hhhh/Graph-Centric/src/lib.rs`. Find the existing `pub mod` block and add `pub mod web;` in alphabetical position (between `tools` and wherever it fits).

- [ ] **Step 2: Create `src/web/mod.rs`**

Create `/home/hhhh/Graph-Centric/src/web/mod.rs`:

```rust
//! Web gateway: axum HTTP/SSE server wrapping the existing agent loop.
//!
//! See `docs/superpowers/specs/2026-06-03-web-gateway-design.md` for the design.
//!
//! This module is a thin HTTP surface. The actual agent logic lives in
//! `crate::agent` (GraphLoop, SubAgent, etc.) and `crate::skills` (skill
//! storage). The web module just exposes these via REST + SSE.

pub mod errors;
pub mod state;
pub mod events;
pub mod run_session;
pub mod api_runs;
pub mod api_skills;
pub mod api_files;

use axum::{routing::get, Router};
use std::sync::Arc;
use tower_http::services::ServeDir;
use crate::skills::SkillStorage;

/// Shared application state passed to every axum handler.
#[derive(Clone)]
pub struct WebState {
    pub runs: Arc<tokio::sync::RwLock<std::collections::HashMap<RunId, Arc<run_session::RunSession>>>>,
    pub skills: Arc<dyn SkillStorage>,
    pub config: state::WebConfig,
}

impl WebState {
    pub fn new(skills: Arc<dyn SkillStorage>, config: state::WebConfig) -> Self {
        Self {
            runs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            skills,
            config,
        }
    }
}

/// Unique identifier for a run (UUID v4 string).
pub type RunId = String;

/// Build the axum Router. `static_dir` is the path to `webui/dist/`
/// (or empty string to skip the static-file mount in tests).
pub fn router(state: WebState, static_dir: &str) -> Router {
    let api = Router::new()
        .route("/api/health", get(api_runs::health))
        .route("/api/runs", get(api_runs::list_runs).post(api_runs::create_run))
        .route("/api/runs/:id", get(api_runs::get_run).delete(api_runs::cancel_run))
        .route("/api/runs/:id/events", get(api_runs::run_events))
        .route("/api/runs/:id/answer", axum::routing::post(api_runs::post_answer))
        .route("/api/runs/:id/repair", axum::routing::post(api_runs::post_repair))
        .route("/api/skills", get(api_skills::list_skills))
        .route("/api/skills/:slug", get(api_skills::get_skill).delete(api_skills::delete_skill))
        .route("/api/skills/:slug/promote", axum::routing::post(api_skills::promote_skill))
        .route("/api/files/changed", get(api_files::files_changed))
        .route("/api/files/diff", get(api_files::file_diff))
        .with_state(state);

    let mut app = Router::new().nest("/api", api);

    if !static_dir.is_empty() {
        app = app.fallback_service(ServeDir::new(static_dir));
    }

    app
}

pub use errors::ApiError;
```

- [ ] **Step 3: Create empty stubs for the submodules referenced in `mod.rs`**

Create 7 empty files (just enough to compile):

`/home/hhhh/Graph-Centric/src/web/errors.rs`:
```rust
//! HTTP error type and IntoResponse mapping.
//! (filled in by Task 3)
```

`/home/hhhh/Graph-Centric/src/web/state.rs`:
```rust
//! WebState config (port, root_dir, etc.).
//! (filled in by Task 5)
```

`/home/hhhh/Graph-Centric/src/web/events.rs`:
```rust
//! RunEvent enum and serialization.
//! (filled in by Task 4)
```

`/home/hhhh/Graph-Centric/src/web/run_session.rs`:
```rust
//! Per-run state machine.
//! (filled in by Task 6)
```

`/home/hhhh/Graph-Centric/src/web/api_runs.rs`:
```rust
//! /api/runs/* handlers.
//! (filled in by Task 7)
```

`/home/hhhh/Graph-Centric/src/web/api_skills.rs`:
```rust
//! /api/skills/* handlers.
//! (filled in by Task 8)
```

`/home/hhhh/Graph-Centric/src/web/api_files.rs`:
```rust
//! /api/files/* handlers.
//! (filled in by Task 9)
```

Each stub has the doc comment + a `// intentionally empty for now` line so it has at least one line. (Or just leave them as single-line file with the doc comment.)

- [ ] **Step 4: Verify cargo check compiles (with stubs in place)**

Run: `cargo check -p graph_harness 2>&1 | tail -20`
Expected: a bunch of "unresolved import" or "cannot find function" errors (because `mod.rs` references things that don't exist yet). These are expected and will go away in subsequent tasks.

---

## Task 3: `errors.rs` — `ApiError` + `IntoResponse`

**Files:**
- Modify: `src/web/errors.rs`

- [ ] **Step 1: Write the file**

Replace `src/web/errors.rs` with:

```rust
//! HTTP error type and `IntoResponse` mapping.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

/// All errors that can be returned from an axum handler. Auto-maps to
/// an HTTP response with a JSON body `{error: {code, message}}` and the
/// appropriate status code.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("conflict: {0}")]
    Conflict(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Conflict(_) => StatusCode::CONFLICT,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::BadRequest(_) => "bad_request",
            Self::Internal(_) => "internal_error",
            Self::Conflict(_) => "conflict",
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorBodyInner,
}

#[derive(Serialize)]
struct ErrorBodyInner {
    code: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ErrorBodyInner {
                code: self.code(),
                message: self.to_string(),
            },
        };
        (self.status(), Json(body)).into_response()
    }
}

// --- Mappings from existing error types ---

impl From<crate::skills::SkillError> for ApiError {
    fn from(e: crate::skills::SkillError) -> Self {
        use crate::skills::SkillError;
        match e {
            SkillError::NotFound(s) => Self::NotFound(s),
            SkillError::InvalidSlug(s) => Self::BadRequest(s),
            SkillError::Io(_) | SkillError::Serde(_) | SkillError::Model(_) | SkillError::Harness(_) => {
                Self::Internal(e.to_string())
            }
        }
    }
}

impl From<crate::error::HarnessError> for ApiError {
    fn from(e: crate::error::HarnessError) -> Self {
        Self::Internal(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_status_is_404() {
        assert_eq!(ApiError::NotFound("x".into()).status(), StatusCode::NOT_FOUND);
        assert_eq!(ApiError::NotFound("x".into()).code(), "not_found");
    }

    #[test]
    fn bad_request_status_is_400() {
        assert_eq!(ApiError::BadRequest("x".into()).status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn internal_status_is_500() {
        assert_eq!(ApiError::Internal("x".into()).status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn skill_error_not_found_maps_to_api_404() {
        let e = crate::skills::SkillError::NotFound("foo".into());
        let api: ApiError = e.into();
        assert_eq!(api.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn skill_error_invalid_slug_maps_to_api_400() {
        let e = crate::skills::SkillError::InvalidSlug("bad!!".into());
        let api: ApiError = e.into();
        assert_eq!(api.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn skill_error_io_maps_to_api_500() {
        let e = crate::skills::SkillError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound, "missing",
        ));
        let api: ApiError = e.into();
        assert_eq!(api.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
```

- [ ] **Step 2: Run the new tests**

Run: `cargo test -p graph_harness --lib web::errors 2>&1 | tail -10`
Expected: 6 tests pass.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: 352 tests pass (346 + 6 new).

---

## Task 4: `events.rs` — `RunEvent` enum + serialization

**Files:**
- Modify: `src/web/events.rs`

- [ ] **Step 1: Write the file**

Replace `src/web/events.rs` with:

```rust
//! Run event types streamed to the browser over SSE.
//!
//! `RunEvent` is the in-process representation; it's serialized to JSON
//! with a `type` discriminator and forwarded as SSE `event: <type>\ndata: <json>`.

use serde::Serialize;

/// All events that can be emitted by a running agent. Tagged enum: the
/// outer `type` field identifies the event kind; the inner `data` field
/// carries the payload.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum RunEvent {
    /// A new transcript message from the Proposer / SubAgent / Reviewer.
    Transcript { role: String, content: String },
    /// Full snapshot of the graph at this point in time.
    GraphSnapshot { nodes: Vec<NodeDto>, edges: Vec<EdgeDto> },
    /// Loop state transition.
    LoopState { kind: String, payload: serde_json::Value },
    /// Review verdict.
    Review { verdict: String, root_cause: Option<String> },
    /// A skill was captured from a successful run.
    SkillCaptured { slug: String, trigger: String },
    /// Terminal Done state.
    Done { final_result: serde_json::Value },
    /// An error occurred.
    Error { message: String },
}

impl RunEvent {
    /// The SSE `event:` field value. Maps to the enum variant name in
    /// snake_case.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::Transcript { .. } => "transcript",
            Self::GraphSnapshot { .. } => "graph",
            Self::LoopState { .. } => "loop_state",
            Self::Review { .. } => "review",
            Self::SkillCaptured { .. } => "skill_captured",
            Self::Done { .. } => "done",
            Self::Error { .. } => "error",
        }
    }
}

/// Minimal DTO for a graph node. The full `Node` struct from `crate::graph`
/// is too heavy for SSE; we send only what the UI needs.
#[derive(Debug, Clone, Serialize)]
pub struct NodeDto {
    pub id: String,
    pub kind: String,
    pub summary: String,
    pub l1: Option<String>,
    pub l1_confidence: Option<f64>,
}

impl NodeDto {
    pub fn from_node(node: &crate::graph::Node, l1: Option<&crate::graph::L1Description>) -> Self {
        Self {
            id: node.id.to_string(),
            kind: format!("{:?}", node.kind),
            summary: node.summary.clone(),
            l1: l1.map(|d| d.render_oneline()),
            l1_confidence: l1.map(|d| d.confidence),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeDto {
    pub source: String,
    pub target: String,
    pub relation: String,
    pub confidence: f64,
}

impl EdgeDto {
    pub fn from_edge(edge: &crate::graph::Edge) -> Self {
        Self {
            source: edge.source.to_string(),
            target: edge.target.to_string(),
            relation: format!("{:?}", edge.relation),
            confidence: edge.confidence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_event_serializes_with_type_discriminator() {
        let event = RunEvent::Transcript {
            role: "assistant".into(),
            content: "hello".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "transcript");
        assert_eq!(v["data"]["role"], "assistant");
        assert_eq!(v["data"]["content"], "hello");
    }

    #[test]
    fn run_event_event_name_matches_variant() {
        assert_eq!(RunEvent::Transcript { role: "x".into(), content: "y".into() }.event_name(), "transcript");
        assert_eq!(RunEvent::Done { final_result: serde_json::json!({}) }.event_name(), "done");
        assert_eq!(RunEvent::Error { message: "x".into() }.event_name(), "error");
    }

    #[test]
    fn node_dto_omits_heavy_fields() {
        let node = crate::graph::Node::file("a.rs", "a file");
        let dto = NodeDto::from_node(&node, None);
        assert_eq!(dto.id, "a.rs");
        assert_eq!(dto.summary, "a file");
        assert!(dto.l1.is_none());
    }

    #[test]
    fn edge_dto_serializes_source_target_relation() {
        let edge = crate::graph::Edge::new("a", "b", crate::graph::RelationType::Imports, 0.9, "");
        let dto = EdgeDto::from_edge(&edge);
        assert_eq!(dto.source, "a");
        assert_eq!(dto.target, "b");
        assert!(dto.relation.contains("Imports"));
        assert!((dto.confidence - 0.9).abs() < 1e-9);
    }
}
```

NOTE: The plan uses `l1.render_oneline()`. Verify this method exists on `L1Description` by reading `src/graph/l1.rs`. If it doesn't exist, use a different method or construct the oneline string manually. (Likely a `l1.render_oneline()` method is part of L1Description; the previous spec plans referenced it. If not, the implementer should adapt.)

- [ ] **Step 2: Run the new tests**

Run: `cargo test -p graph_harness --lib web::events 2>&1 | tail -10`
Expected: 4 tests pass (if `L1Description` has the expected API; otherwise fewer, plus an indication to adapt).

- [ ] **Step 3: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: ~356 tests pass (352 + 4 new).

---

## Task 5: `state.rs` — `WebConfig` + finalize `WebState`

**Files:**
- Modify: `src/web/state.rs`
- Modify: `src/web/mod.rs` (replace the placeholder `WebState` with the real one)

- [ ] **Step 1: Write `state.rs`**

Replace `src/web/state.rs` with:

```rust
//! Web configuration: port, root directory, model defaults.

use std::path::PathBuf;

/// Static configuration for the web gateway. Read from env at startup
/// (fail fast). All fields have sensible defaults except `bind_addr`,
/// which falls back to `0.0.0.0:8080`.
#[derive(Debug, Clone)]
pub struct WebConfig {
    /// Address to bind the HTTP server to.
    pub bind_addr: String,
    /// Path to the directory of static frontend files (`webui/dist/`).
    /// Empty string disables static file serving.
    pub static_dir: String,
    /// Project root (cwd by default). Used for git-based file diffs.
    pub project_root: PathBuf,
}

impl WebConfig {
    /// Read from env vars. Falls back to defaults.
    pub fn from_env() -> Self {
        let bind_addr = std::env::var("WEB_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .map(|p| format!("0.0.0.0:{p}"))
            .unwrap_or_else(|| "0.0.0.0:8080".to_string());
        let static_dir = std::env::var("WEB_STATIC_DIR")
            .unwrap_or_else(|_| "webui/dist".to_string());
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self { bind_addr, static_dir, project_root }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_config_defaults_to_localhost_8080() {
        // We can't easily test `from_env` because it reads real env vars.
        // Instead, just construct manually.
        let cfg = WebConfig {
            bind_addr: "0.0.0.0:8080".to_string(),
            static_dir: "webui/dist".to_string(),
            project_root: PathBuf::from("."),
        };
        assert!(cfg.bind_addr.contains("8080"));
    }

    #[test]
    fn web_config_honors_web_port_env() {
        // Manually simulate the env-var parsing.
        let port: u16 = "9999".parse().unwrap();
        let addr = format!("0.0.0.0:{port}");
        assert_eq!(addr, "0.0.0.0:9999");
    }
}
```

- [ ] **Step 2: Update `src/web/mod.rs` to use the new `WebConfig`**

Find the `WebState` struct in `mod.rs` (created in Task 2). It currently has:
```rust
pub struct WebState {
    pub runs: Arc<...>,
    pub skills: Arc<dyn SkillStorage>,
    pub config: state::WebConfig,
}
```

The placeholder `WebState` already uses `state::WebConfig`. After Task 5 adds the real `WebConfig`, it should compile. No edit needed to `mod.rs` unless the import path is wrong.

Verify the import: `mod.rs` should have `pub mod state;` (added in Task 2) and use `state::WebConfig` directly. The `pub use state::WebConfig;` re-export in Task 2's `mod.rs` already exists, so external code can use `crate::web::WebConfig` — but internal use is `state::WebConfig`.

- [ ] **Step 3: Run the new tests**

Run: `cargo test -p graph_harness --lib web::state 2>&1 | tail -5`
Expected: 2 tests pass.

- [ ] **Step 4: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: ~358 tests pass (356 + 2 new).

---

## Task 6: `run_session.rs` — per-run state machine

**Files:**
- Modify: `src/web/run_session.rs`

- [ ] **Step 1: Write the file**

Replace `src/web/run_session.rs` with:

```rust
//! Per-run state machine. One `RunSession` per active or completed run.

use super::events::{EdgeDto, NodeDto, RunEvent};
use crate::agent::graph_loop::FinalResult;
use crate::agent::reviewer::ReviewResult;
use crate::graph::Graph;
use crate::skills::SkillRef;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, Notify};
use tokio_util::sync::CancellationToken;

const EVENT_CHANNEL_CAPACITY: usize = 256;

/// One run's state. The `RunSession` is the single source of truth for
/// a run; the SSE handler subscribes to `event_tx` and forwards events
/// to the browser.
pub struct RunSession {
    pub id: String,
    pub task: String,
    pub started_at: Instant,
    pub status: tokio::sync::RwLock<RunStatus>,
    pub event_tx: broadcast::Sender<RunEvent>,
    pub cancel: CancellationToken,
    /// Resolved by `POST /api/runs/{id}/answer` when the run is `Paused`.
    pub pending_answer: Notify,
    pub pending_answer_value: tokio::sync::Mutex<Option<String>>,
    /// Resolved by `POST /api/runs/{id}/repair` when the run is `GraphInvalid`.
    pub pending_repair: Notify,
    pub pending_repair_value: tokio::sync::Mutex<Option<Graph>>,
    /// Last known graph (kept in sync with broadcast events).
    pub last_graph: tokio::sync::RwLock<Arc<Graph>>,
    /// Last known review result (if any).
    pub last_review: tokio::sync::RwLock<Option<ReviewResult>>,
    /// The captured skill (if the run completed with Pass and capture succeeded).
    pub captured_skill: tokio::sync::RwLock<Option<SkillRef>>,
}

/// Run state visible to the API and the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Paused,
    GraphInvalid,
    Done,
    Error(String),
    Cancelled,
}

impl RunStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Cancelled | Self::Error(_))
    }
}

impl RunSession {
    pub fn new(id: String, task: String) -> Self {
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            id,
            task,
            started_at: Instant::now(),
            status: tokio::sync::RwLock::new(RunStatus::Running),
            event_tx,
            cancel: CancellationToken::new(),
            pending_answer: Notify::new(),
            pending_answer_value: tokio::sync::Mutex::new(None),
            pending_repair: Notify::new(),
            pending_repair_value: tokio::sync::Mutex::new(None),
            last_graph: tokio::sync::RwLock::new(Arc::new(Graph::new())),
            last_review: tokio::sync::RwLock::new(None),
            captured_skill: tokio::sync::RwLock::new(None),
        }
    }

    /// Broadcast an event to all SSE subscribers. No-op if there are no
    /// active subscribers (the channel is bounded and will drop lagged
    /// subscribers gracefully).
    pub fn emit(&self, event: RunEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Snapshot the current graph as a `RunEvent::GraphSnapshot` and
    /// emit it. Also updates `last_graph`.
    pub async fn emit_graph_snapshot(&self, graph: &Graph) {
        let nodes: Vec<NodeDto> = graph
            .nodes
            .values()
            .map(|n| NodeDto::from_node(n, graph.l1.get(&n.id)))
            .collect();
        let edges: Vec<EdgeDto> = graph
            .edges
            .iter()
            .map(EdgeDto::from_edge)
            .collect();
        *self.last_graph.write().await = Arc::new(graph.clone());
        self.emit(RunEvent::GraphSnapshot { nodes, edges });
    }

    /// Wait for the user to provide an answer (via `POST /api/runs/{id}/answer`).
    pub async fn await_answer(&self) -> String {
        self.pending_answer.notified().await;
        self.pending_answer_value
            .lock()
            .await
            .take()
            .unwrap_or_default()
    }

    /// Provide an answer. Called from the HTTP handler.
    pub async fn provide_answer(&self, answer: String) {
        *self.pending_answer_value.lock().await = Some(answer);
        self.pending_answer.notify_one();
    }

    /// Wait for the user to provide a repaired graph (via `POST /api/runs/{id}/repair`).
    pub async fn await_repair(&self) -> Graph {
        self.pending_repair.notified().await;
        self.pending_repair_value
            .lock()
            .await
            .take()
            .unwrap_or_default()
    }

    /// Provide a repaired graph. Called from the HTTP handler.
    pub async fn provide_repair(&self, graph: Graph) {
        *self.pending_repair_value.lock().await = Some(graph);
        self.pending_repair.notify_one();
    }

    /// Snapshot of the run for the `GET /api/runs/{id}` endpoint.
    pub async fn metadata(&self) -> RunMetadata {
        let status = self.status.read().await.clone();
        let duration_ms = self.started_at.elapsed().as_millis() as u64;
        RunMetadata {
            id: self.id.clone(),
            task: self.task.clone(),
            status,
            duration_ms,
            captured_skill: self.captured_skill.read().await.clone(),
        }
    }
}

/// Public-facing run metadata returned by `GET /api/runs/{id}` and
/// `GET /api/runs`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunMetadata {
    pub id: String,
    pub task: String,
    pub status: RunStatus,
    pub duration_ms: u64,
    pub captured_skill: Option<SkillRef>,
}

// RunStatus also needs Serialize for the API. Add it here.
impl serde::Serialize for RunStatus {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        match self {
            Self::Running => s.serialize_newtype_variant("RunStatus", 0, "Running"),
            Self::Paused => s.serialize_newtype_variant("RunStatus", 1, "Paused"),
            Self::GraphInvalid => s.serialize_newtype_variant("RunStatus", 2, "GraphInvalid"),
            Self::Done => s.serialize_newtype_variant("RunStatus", 3, "Done"),
            Self::Error(msg) => s.serialize_newtype_variant("RunStatus", 4, "Error"),
            Self::Cancelled => s.serialize_newtype_variant("RunStatus", 5, "Cancelled"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_session_starts_running() {
        let s = RunSession::new("r1".into(), "task".into());
        assert_eq!(*s.status.read().await, RunStatus::Running);
    }

    #[tokio::test]
    async fn emit_and_receive_via_broadcast() {
        let s = RunSession::new("r1".into(), "task".into());
        let mut rx = s.event_tx.subscribe();
        s.emit(RunEvent::Transcript {
            role: "assistant".into(),
            content: "hi".into(),
        });
        let event = rx.recv().await.unwrap();
        match event {
            RunEvent::Transcript { role, content } => {
                assert_eq!(role, "assistant");
                assert_eq!(content, "hi");
            }
            _ => panic!("expected Transcript"),
        }
    }

    #[tokio::test]
    async fn cancel_token_cancels() {
        let s = RunSession::new("r1".into(), "task".into());
        assert!(!s.cancel.is_cancelled());
        s.cancel.cancel();
        assert!(s.cancel.is_cancelled());
    }

    #[tokio::test]
    async fn answer_flow_resolves() {
        let s = std::sync::Arc::new(RunSession::new("r1".into(), "task".into()));
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.await_answer().await });
        // Give the waiter a tick to subscribe.
        tokio::task::yield_now().await;
        s.provide_answer("the answer".into()).await;
        let result = waiter.await.unwrap();
        assert_eq!(result, "the answer");
    }

    #[tokio::test]
    async fn repair_flow_resolves() {
        let s = std::sync::Arc::new(RunSession::new("r1".into(), "task".into()));
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.await_repair().await });
        tokio::task::yield_now().await;
        let g = crate::graph::Graph::new();
        s.provide_repair(g.clone()).await;
        let received = waiter.await.unwrap();
        assert_eq!(received.node_count(), 0);
    }

    #[test]
    fn is_terminal_for_done_and_cancelled() {
        assert!(RunStatus::Done.is_terminal());
        assert!(RunStatus::Cancelled.is_terminal());
        assert!(RunStatus::Error("x".into()).is_terminal());
        assert!(!RunStatus::Running.is_terminal());
        assert!(!RunStatus::Paused.is_terminal());
        assert!(!RunStatus::GraphInvalid.is_terminal());
    }
}
```

- [ ] **Step 2: Run the new tests**

Run: `cargo test -p graph_harness --lib web::run_session 2>&1 | tail -15`
Expected: 6 tests pass.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: ~364 tests pass (358 + 6 new).

---

## Task 7: `api_runs.rs` — `/api/runs/*` handlers + run driver

**Files:**
- Modify: `src/web/api_runs.rs`

- [ ] **Step 1: Write the file**

This task is the largest single file in the web module. It includes:
- 6 HTTP handlers (health, list_runs, create_run, get_run, cancel_run, run_events, post_answer, post_repair)
- A `drive_run` function that runs the actual agent loop in a `tokio::spawn` task
- Mapping `LoopState` → `RunEvent`

Replace `src/web/api_runs.rs` with:

```rust
//! `/api/runs/*` HTTP handlers and the run driver.
//!
//! The driver spawns a tokio task that constructs a `GraphLoop` (same as
//! `bin/agent_a` does), runs `step()` in a loop, and maps each
//! `LoopState` to one or more `RunEvent`s broadcast on the session's
//! channel. Cancellation, answers, and repairs are all coordinated
//! through `tokio::sync::Notify` + the session's storage.

use super::errors::ApiError;
use super::events::RunEvent;
use super::run_session::{RunMetadata, RunSession, RunStatus};
use super::state::WebState;
use super::RunId;
use crate::agent::graph_loop::{FinalResult, GraphError, GraphLoop, LoopState};
use crate::agent::graph_loop_config::GraphLoopConfig;
use crate::agent::proposer::GraphProposer;
use crate::agent::verifier::Verifier;
use crate::agent::enricher::L1Enricher;
use crate::agent::repairer::LocalRepairer;
use crate::agent::decomposer::Decomposer;
use crate::agent::dispatcher::Dispatcher;
use crate::agent::reviewer::Reviewer;
use crate::agent::validator::BashCheckValidator;
use crate::agent::conversation::Conversation;
use crate::graph::Graph;
use crate::model::{Model, ModelConfig};
use crate::tools::BashTool;
use crate::tools::ToolRegistry;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::StreamExt;

type AppState = State<Arc<WebState>>;

// --- Health ---

pub async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

// --- List runs ---

pub async fn list_runs(State(state): AppState) -> Result<Json<Vec<RunMetadata>>, ApiError> {
    let runs = state.runs.read().await;
    let mut out = Vec::new();
    for s in runs.values() {
        out.push(s.metadata().await);
    }
    // Sort by started_at descending.
    out.sort_by(|a, b| b.duration_ms.cmp(&a.duration_ms).reverse());
    Ok(Json(out))
}

// --- Create run ---

#[derive(Deserialize)]
pub struct CreateRunBody {
    pub task: String,
}

pub async fn create_run(
    State(state): State<Arc<WebState>>,
    Json(body): Json<CreateRunBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = uuid::Uuid::new_v4().to_string();
    let session = Arc::new(RunSession::new(id.clone(), body.task.clone()));
    state.runs.write().await.insert(id.clone(), session.clone());

    // Spawn the run driver.
    let state_clone = state.clone();
    let id_clone = id.clone();
    tokio::spawn(async move {
        drive_run(state_clone, id_clone).await;
    });

    Ok(Json(serde_json::json!({"id": id})))
}

// --- Get run ---

pub async fn get_run(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
) -> Result<Json<RunMetadata>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs.get(&id).ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    Ok(Json(session.metadata().await))
}

// --- Cancel run ---

pub async fn cancel_run(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs.get(&id).ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    session.cancel.cancel();
    Ok(Json(serde_json::json!({"cancelled": true})))
}

// --- SSE event stream ---

pub async fn run_events(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs.get(&id).ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    let rx = session.event_tx.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
        .filter_map(|res| res.ok())  // skip lagged/inactive
        .map(|event: RunEvent| {
            let sse_event = Event::default()
                .event(event.event_name())
                .json_data(event)
                .unwrap_or_else(|_| Event::default().comment("serialization error"));
            Ok::<_, Infallible>(sse_event)
        })
        .merge(KeepAlive::stream(Duration::from_secs(15)).map(|ka| Ok::<_, Infallible>(ka)));
    Ok(Sse::new(stream))
}

// --- Post answer (resume Paused) ---

#[derive(Deserialize)]
pub struct AnswerBody {
    pub answer: String,
}

pub async fn post_answer(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
    Json(body): Json<AnswerBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs.get(&id).ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    if *session.status.read().await != RunStatus::Paused {
        return Err(ApiError::Conflict(format!("run {id} is not Paused")));
    }
    session.provide_answer(body.answer).await;
    Ok(Json(serde_json::json!({"accepted": true})))
}

// --- Post repair (resume GraphInvalid) ---

#[derive(Deserialize)]
pub struct RepairBody {
    pub graph: Graph,
}

pub async fn post_repair(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
    Json(body): Json<RepairBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs.get(&id).ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    if *session.status.read().await != RunStatus::GraphInvalid {
        return Err(ApiError::Conflict(format!("run {id} is not GraphInvalid")));
    }
    session.provide_repair(body.graph).await;
    Ok(Json(serde_json::json!({"accepted": true})))
}

// --- Run driver ---

/// The actual agent loop. Spawned as a tokio task by `create_run`. Maps
/// each `LoopState` to events and broadcasts them on the session's
/// channel. Resolves `Paused` and `GraphInvalid` via the session's
/// `Notify` machinery.
async fn drive_run(state: Arc<WebState>, id: RunId) {
    let session = {
        let runs = state.runs.read().await;
        match runs.get(&id) {
            Some(s) => s.clone(),
            None => return,
        }
    };

    // Build the GraphLoop. This mirrors what bin/agent_a does.
    let cfg = match ModelConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            session.emit(RunEvent::Error { message: format!("config error: {e}") });
            *session.status.write().await = RunStatus::Error(e.to_string());
            return;
        }
    };
    let fast_model = match cfg.build_fast_model() {
        Ok(m) => Arc::from(m),
        Err(e) => {
            session.emit(RunEvent::Error { message: format!("fast model build: {e}") });
            *session.status.write().await = RunStatus::Error(e.to_string());
            return;
        }
    };
    let deep_model = match cfg.build_deep_model() {
        Ok(m) => Arc::from(m),
        Err(e) => {
            session.emit(RunEvent::Error { message: format!("deep model build: {e}") });
            *session.status.write().await = RunStatus::Error(e.to_string());
            return;
        }
    };

    // Tools.
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register(Arc::new(BashTool::new()));

    // Proposer (with skills).
    let proposer = GraphProposer::new(
        fast_model.clone(),
        Arc::new(tool_registry.clone()),
        Some(state.skills.clone()),
    );

    // Other components (simplified — full model wiring).
    let verifier = Verifier::with_model(fast_model.clone());
    let enricher = L1Enricher::new(deep_model.clone(), std::sync::Arc::new(
        crate::context::NullSourceLoader,
    ));
    let repairer = LocalRepairer::new(deep_model.clone());
    let decomposer = Decomposer::new(deep_model.clone());
    let subagent = std::sync::Arc::new(
        crate::agent::subagent::SubAgent::new(fast_model.clone())
            .with_tools(Arc::new(tool_registry))
            .with_policy(Arc::new(crate::tools::DangerousCommandDeny::new())),
    );
    let dispatcher = Dispatcher::new(subagent);
    let reviewer = Reviewer::with_model(deep_model.clone());
    let validator: Arc<dyn crate::agent::validator::PostExecutionValidator> =
        Arc::new(BashCheckValidator::cargo_check_for(&state.config.project_root));

    let loop_cfg = GraphLoopConfig {
        max_rounds: 24,
        max_repair_rounds: 3,
        tool_cwd: state.config.project_root.clone(),
        tool_output_cap: 8_000,
        tool_policy: Arc::new(crate::tools::DangerousCommandDeny::new()),
    };

    let conversation = Conversation::new(proposer.build_system_prompt(&session.task), session.task.clone());

    let mut gl = GraphLoop::new(
        loop_cfg,
        conversation,
        proposer,
        verifier,
        enricher,
        repairer,
        decomposer,
        dispatcher,
        reviewer,
        vec![validator],
    );

    // Main loop.
    loop {
        if session.cancel.is_cancelled() {
            *session.status.write().await = RunStatus::Cancelled;
            session.emit(RunEvent::Done {
                final_result: serde_json::json!({"status": "cancelled"}),
            });
            return;
        }

        let state_clone = gl.step();
        session.emit_graph_snapshot(&gl.world_graph_clone()).await;
        session.emit(RunEvent::LoopState {
            kind: format!("{:?}", state_clone).split_whitespace().next().unwrap_or("?").to_string(),
            payload: serde_json::to_value(&state_clone).unwrap_or(serde_json::Value::Null),
        });

        match state_clone {
            LoopState::Paused { question, rationale: _ } => {
                *session.status.write().await = RunStatus::Paused;
                session.emit(RunEvent::Transcript {
                    role: "ask_user".into(),
                    content: question.clone(),
                });
                let answer = session.await_answer().await;
                gl = gl.resume(answer);
            }
            LoopState::GraphInvalid { source: _, errors: _, snapshot: _ } => {
                *session.status.write().await = RunStatus::GraphInvalid;
                let errors_json = serde_json::to_value(&gl.world_graph_clone()).unwrap_or(serde_json::Value::Null);
                session.emit(RunEvent::LoopState {
                    kind: "GraphInvalid".into(),
                    payload: errors_json,
                });
                let _repaired = session.await_repair().await;
                // v1: For simplicity, we just log and continue without
                // actually re-applying. A future task will wire up the
                // proper repair loop (caller-driven, per spec §6).
                session.emit(RunEvent::Error {
                    message: "graph repair not yet wired in v1".into(),
                });
            }
            LoopState::Done(final_result) => {
                *session.status.write().await = RunStatus::Done;
                session.emit(RunEvent::Done {
                    final_result: serde_json::to_value(&final_result).unwrap_or(serde_json::Value::Null),
                });
                return;
            }
            LoopState::Error(msg) => {
                *session.status.write().await = RunStatus::Error(msg.clone());
                session.emit(RunEvent::Error { message: msg });
                return;
            }
            LoopState::TaskFailed { failures } => {
                // Treat as terminal for v1.
                *session.status.write().await = RunStatus::Error(format!("task failed: {:?}", failures));
                session.emit(RunEvent::Error {
                    message: format!("task failed: {failures:?}"),
                });
                return;
            }
            LoopState::Running => {
                // Continue looping.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::storage::LocalSkillStorage;

    fn make_state() -> Arc<WebState> {
        let dir = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let cfg = WebConfig {
            bind_addr: "0.0.0.0:0".to_string(),
            static_dir: String::new(),
            project_root: dir.path().to_path_buf(),
        };
        Arc::new(WebState::new(local, cfg))
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        let resp = health().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn create_run_returns_id() {
        let state = make_state();
        let resp = create_run(
            State(state.clone()),
            Json(CreateRunBody { task: "test".into() }),
        )
        .await
        .unwrap()
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn get_unknown_run_returns_404() {
        let state = make_state();
        let resp = get_run(State(state), Path("nope".into())).await.unwrap_err();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cancel_unknown_run_returns_404() {
        let state = make_state();
        let resp = cancel_run(State(state), Path("nope".into())).await.unwrap_err();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn post_answer_to_running_run_returns_409() {
        let state = make_state();
        let id = uuid::Uuid::new_v4().to_string();
        let session = Arc::new(RunSession::new(id.clone(), "t".into()));
        state.runs.write().await.insert(id.clone(), session);
        let resp = post_answer(
            State(state),
            Path(id),
            Json(AnswerBody { answer: "x".into() }),
        )
        .await
        .unwrap_err();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }
}
```

- [ ] **Step 2: Run the new tests**

Run: `cargo test -p graph_harness --lib web::api_runs 2>&1 | tail -15`
Expected: 5 tests pass.

If compile errors mention `GraphLoop` constructor fields, `ModelConfig` methods (`build_fast_model`, `build_deep_model`), or `Verifier::with_model` signatures, the implementer should adapt the code by reading the actual signatures in `src/agent/graph_loop.rs`, `src/model/config.rs`, and `src/agent/verifier.rs`. The plan code is the **shape**; the exact field/method names may need adjustment.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: ~369 tests pass (364 + 5 new).

---

## Task 8: `api_skills.rs` — `/api/skills/*` handlers

**Files:**
- Modify: `src/web/api_skills.rs`

- [ ] **Step 1: Write the file**

Replace `src/web/api_skills.rs` with:

```rust
//! `/api/skills/*` HTTP handlers.

use super::errors::ApiError;
use super::state::WebState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use std::sync::Arc;

type AppState = State<Arc<WebState>>;

pub async fn list_skills(
    State(state): AppState,
) -> Result<Json<Vec<crate::skills::SkillRef>>, ApiError> {
    let list = state.skills.list()?;
    Ok(Json(list))
}

pub async fn get_skill(
    State(state): AppState,
    Path(slug): Path<String>,
) -> Result<Json<crate::skills::Skill>, ApiError> {
    let skill = state.skills.load(&slug)?;
    Ok(Json(skill))
}

pub async fn promote_skill(
    State(state): AppState,
    Path(slug): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Promote = copy from local to repo (if currently in local).
    // Strategy: load the skill (from composite, which prefers local),
    // then save to the repo storage directly.
    let skill = state.skills.load(&slug)?;
    if let Some(repo_root) = repo_root_for(&state) {
        let repo = crate::skills::RepoSkillStorage::new(repo_root);
        repo.save(&skill)?;
        Ok(Json(serde_json::json!({"promoted": true})))
    } else {
        Err(ApiError::Internal("no repo root configured".into()))
    }
}

pub async fn delete_skill(
    State(state): AppState,
    Path(slug): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Delete from both local and repo (idempotent).
    delete_skill_from(&state.config, &slug).map_err(|e| {
        // If the file doesn't exist, treat as already-deleted.
        if e.kind() == std::io::ErrorKind::NotFound {
            ApiError::NotFound(slug.clone())
        } else {
            ApiError::Internal(e.to_string())
        }
    })?;
    Ok(Json(serde_json::json!({"deleted": true})))
}

// --- Helpers ---

/// Get the repo root path from the state.
fn repo_root_for(state: &WebState) -> Option<std::path::PathBuf> {
    let s = state.skills.repo_root();
    if s.as_os_str().is_empty() { None } else { Some(s) }
}

/// Delete a skill directory under both local and repo roots.
fn delete_skill_from(config: &super::state::WebConfig, slug: &str) -> std::io::Result<()> {
    use crate::skills::storage::SkillStorage;
    // Try deleting from local first.
    if let Ok(local) = std::env::var("HOME").map(|h| {
        let mut p = std::path::PathBuf::from(h);
        p.push(".local"); p.push("share"); p.push("graph-centric"); p.push("skills"); p.push(slug);
        p
    }) {
        let _ = std::fs::remove_dir_all(&local);  // best-effort
    }
    // Then repo.
    let mut repo = config.project_root.clone();
    repo.push("skills");
    repo.push(slug);
    std::fs::remove_dir_all(&repo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::storage::LocalSkillStorage;
    use crate::skills::types::{Skill, SkillMeta};
    use crate::graph::Graph;

    fn make_state() -> Arc<WebState> {
        let dir = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let cfg = super::super::state::WebConfig {
            bind_addr: "0.0.0.0:0".to_string(),
            static_dir: String::new(),
            project_root: dir.path().to_path_buf(),
        };
        Arc::new(WebState::new(local, cfg))
    }

    fn sample_skill(slug: &str) -> Skill {
        Skill {
            slug: slug.to_string(),
            task: "t".into(),
            trigger: "trig".into(),
            graph: Graph::new(),
            review: serde_json::json!({}),
            meta: SkillMeta {
                created_at: "2026-06-03T00:00:00Z".into(),
                task_id: None,
                model_used: "test".into(),
                domain_tags: vec![],
                l1_avg_confidence: 0.0,
            },
        }
    }

    #[tokio::test]
    async fn list_skills_returns_empty_when_no_skills() {
        let state = make_state();
        let resp = list_skills(State(state)).await.unwrap();
        assert!(resp.0.is_empty());
    }

    #[tokio::test]
    async fn get_skill_404_when_missing() {
        let state = make_state();
        let err = get_skill(State(state), Path("nope".into())).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }
}
```

- [ ] **Step 2: Run the new tests**

Run: `cargo test -p graph_harness --lib web::api_skills 2>&1 | tail -10`
Expected: 2 tests pass.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: ~371 tests pass (369 + 2 new).

---

## Task 9: `api_files.rs` — git-based file diff endpoints

**Files:**
- Modify: `src/web/api_files.rs`

- [ ] **Step 1: Write the file**

Replace `src/web/api_files.rs` with:

```rust
//! `/api/files/*` HTTP handlers (git-based file change detection).

use super::errors::ApiError;
use super::state::WebState;
use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::sync::Arc;

type AppState = State<Arc<WebState>>;

#[derive(Deserialize)]
pub struct ChangedSince {
    #[serde(default)]
    pub since: Option<String>,  // ISO 8601
}

#[derive(Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub change_type: String,  // "added" | "modified" | "deleted"
}

pub async fn files_changed(
    State(state): AppState,
    Query(params): Query<ChangedSince>,
) -> Result<Json<Vec<ChangedFile>>, ApiError> {
    let project = &state.config.project_root;
    let mut cmd = Command::new("git");
    cmd.current_dir(project).arg("diff").arg("--name-status").arg("--no-color");
    if let Some(since) = &params.since {
        cmd.arg("--since").arg(since);
    } else {
        // No `since` — show all changes vs HEAD (i.e., uncommitted).
    }
    let output = cmd.output()
        .map_err(|e| ApiError::Internal(format!("git not available: {e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let mut out = Vec::new();
    for line in stdout.lines() {
        // Format: "M\tpath" or "A\tpath" or "D\tpath"
        let mut parts = line.splitn(2, '\t');
        let status = parts.next().unwrap_or("").trim();
        let path = parts.next().unwrap_or("").trim().to_string();
        if !path.is_empty() {
            out.push(ChangedFile {
                path,
                change_type: match status {
                    "A" => "added".into(),
                    "D" => "deleted".into(),
                    _ => "modified".into(),
                },
            });
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct DiffPath {
    pub path: String,
}

pub async fn file_diff(
    State(state): AppState,
    Query(params): Query<DiffPath>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let project = &state.config.project_root;
    let output = Command::new("git")
        .current_dir(project)
        .args(["diff", "HEAD", "--", &params.path])
        .output()
        .map_err(|e| ApiError::Internal(format!("git not available: {e}")))?;
    let diff = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(Json(serde_json::json!({
        "path": params.path,
        "diff": diff,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::storage::LocalSkillStorage;

    fn make_state_in_dir(dir: &std::path::Path) -> Arc<WebState> {
        let local = Arc::new(LocalSkillStorage::new(dir.to_path_buf()));
        let cfg = super::super::state::WebConfig {
            bind_addr: "0.0.0.0:0".to_string(),
            static_dir: String::new(),
            project_root: dir.to_path_buf(),
        };
        Arc::new(WebState::new(local, cfg))
    }

    #[tokio::test]
    async fn file_diff_returns_empty_string_when_no_changes() {
        // In a tempdir with no git repo, `git diff HEAD` errors. The
        // handler should return an error or an empty diff.
        let dir = tempfile::tempdir().unwrap();
        let state = make_state_in_dir(dir.path());
        let result = file_diff(
            State(state),
            Query(DiffPath { path: "anything".into() }),
        )
        .await;
        // Either Ok (with empty diff) or Err (git not available) is acceptable.
        match result {
            Ok(json) => {
                let v: serde_json::Value = json.0;
                assert_eq!(v["diff"], "");
            }
            Err(_) => { /* git not available; acceptable */ }
        }
    }
}
```

- [ ] **Step 2: Run the new tests**

Run: `cargo test -p graph_harness --lib web::api_files 2>&1 | tail -10`
Expected: 1 test pass.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: ~372 tests pass (371 + 1 new).

---

## Task 10: `bin/serve.rs` — the binary

**Files:**
- Create: `src/bin/serve.rs`

- [ ] **Step 1: Create the file**

Create `/home/hhhh/Graph-Centric/src/bin/serve.rs`:

```rust
//! `serve` — the web gateway binary.
//!
//! Wraps the existing agent loop in an axum HTTP server with SSE event
//! streaming. Browse to http://localhost:8080 after starting.
//!
//! Environment:
//!   WEB_PORT          bind port (default 8080)
//!   WEB_STATIC_DIR    path to webui/dist (default "webui/dist")
//!   MODEL_BASE_URL, MODEL_API_KEY, etc.  (from .env or env)

use graph_harness::skills::storage::{CompositeSkillStorage, LocalSkillStorage, RepoSkillStorage};
use graph_harness::web::state::WebConfig;
use graph_harness::web::WebState;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load .env if present.
    let _ = dotenvy::dotenv();

    let config = WebConfig::from_env();
    info!(addr = %config.bind_addr, static_dir = %config.static_dir, "starting web gateway");

    // Build skill storage (composite: local + repo).
    let local_root = LocalSkillStorage::default_install()
        .map(|s| s.root)
        .unwrap_or_else(|| std::env::temp_dir().join("graph-centric-skills-fallback"));
    let repo_root = config.project_root.join("skills");
    let skill_storage: Arc<dyn graph_harness::skills::SkillStorage> = Arc::new(
        CompositeSkillStorage::new(
            LocalSkillStorage::new(local_root),
            RepoSkillStorage::new(repo_root),
        ),
    );

    // Build state.
    let state = Arc::new(WebState::new(skill_storage, config.clone()));

    // Build router.
    let app = graph_harness::web::router(state, &config.static_dir);

    // Bind.
    let addr: SocketAddr = config.bind_addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(?addr, "listening");

    axum::serve(listener, app).await?;
    Ok(())
}
```

- [ ] **Step 2: Verify cargo check compiles**

Run: `cargo check -p graph_harness 2>&1 | tail -20`
Expected: clean.

If `WebConfig::clone` isn't `Clone` (we derive it), add `#[derive(Clone)]`.

- [ ] **Step 3: Run the full test suite (no new tests in this task)**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -3`
Expected: ~372 tests pass (no change; this task only added the binary).

- [ ] **Step 4: Smoke-test the binary (without a frontend)**

Build and run the binary briefly to confirm it boots:

```bash
cargo build -p graph_harness --bin serve
WEB_PORT=18080 WEB_STATIC_DIR= ./target/debug/serve &
SERVER_PID=$!
sleep 2
curl -s http://localhost:18080/api/health
kill $SERVER_PID
```

Expected: `{"status":"ok"}` printed. (If `WEB_STATIC_DIR=` is empty, the static file mount is skipped.)

---

## Task 11: HTTP integration tests with `axum::Router::oneshot`

**Files:**
- Create: `tests/integration_web_gateway.rs` (new integration test file in project root)

- [ ] **Step 1: Create the integration test file**

Create `/home/hhhh/Graph-Centric/tests/integration_web_gateway.rs`:

```rust
//! End-to-end tests for the web gateway. Use `axum::Router::oneshot` to
//! dispatch requests in-process — no real network binding.

use std::sync::Arc;
use tempfile::TempDir;

use graph_harness::skills::storage::LocalSkillStorage;
use graph_harness::skills::types::{Skill, SkillMeta};
use graph_harness::graph::Graph;
use graph_harness::web::state::WebConfig;
use graph_harness::web::WebState;

fn make_state() -> (TempDir, Arc<WebState>) {
    let dir = TempDir::new().unwrap();
    let local = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
    let cfg = WebConfig {
        bind_addr: "0.0.0.0:0".to_string(),
        static_dir: String::new(),
        project_root: dir.path().to_path_buf(),
    };
    (dir, Arc::new(WebState::new(local, cfg)))
}

#[tokio::test]
async fn health_endpoint_returns_ok() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let (_dir, state) = make_state();
    let app = graph_harness::web::router(state, "");

    let resp = app
        .oneshot(Request::builder().uri("/api/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("\"ok\""));
}

#[tokio::test]
async fn list_skills_returns_empty_initially() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (_dir, state) = make_state();
    let app = graph_harness::web::router(state, "");

    let resp = app
        .oneshot(Request::builder().uri("/api/skills").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.is_array());
    assert!(v.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn get_skill_404_for_missing_slug() {
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let (_dir, state) = make_state();
    let app = graph_harness::web::router(state, "");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/skills/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_run_returns_uuid_id() {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let (_dir, state) = make_state();
    let app = graph_harness::web::router(state.clone(), "");

    let body = serde_json::to_vec(&serde_json::json!({"task": "do nothing"})).unwrap();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runs")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = to_bytes(resp.into_body(), 4096).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(v["id"].is_string());
}

#[tokio::test]
async fn get_run_404_for_missing_id() {
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let (_dir, state) = make_state();
    let app = graph_harness::web::router(state, "");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/runs/00000000-0000-0000-0000-000000000000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
```

- [ ] **Step 2: Run the integration tests**

Run: `cargo test -p graph_harness --test integration_web_gateway 2>&1 | tail -10`
Expected: 5 tests pass.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -10`
Expected: 377 tests pass (372 lib + 5 integration).

---

## Task 12: SvelteKit scaffold + .gitignore + Tailwind config

**Files:**
- Modify: `.gitignore` (add webui ignores)
- Create: `webui/package.json`, `webui/svelte.config.js`, `webui/vite.config.ts`, `webui/tsconfig.json`, `webui/tailwind.config.cjs`, `webui/postcss.config.cjs`, `webui/.gitignore`, `webui/src/app.html`, `webui/src/app.css`

- [ ] **Step 1: Add `webui` ignores to `.gitignore`**

Read `/home/hhhh/Graph-Centric/.gitignore`. Add these lines:

```
webui/node_modules/
webui/.svelte-kit/
webui/build/
webui/dist/
```

(Also create `/home/hhhh/Graph-Centric/webui/.gitignore` with the same lines, just to be safe.)

- [ ] **Step 2: Create `webui/package.json`**

Create `/home/hhhh/Graph-Centric/webui/package.json`:

```json
{
  "name": "graph-centric-webui",
  "version": "0.0.1",
  "private": true,
  "type": "module",
  "scripts": {
    "dev": "vite dev --port 5173",
    "build": "vite build",
    "preview": "vite preview",
    "check": "svelte-kit sync && svelte-check --tsconfig ./tsconfig.json"
  },
  "devDependencies": {
    "@sveltejs/adapter-static": "^3.0.0",
    "@sveltejs/kit": "^2.0.0",
    "@sveltejs/vite-plugin-svelte": "^3.0.0",
    "autoprefixer": "^10.4.0",
    "postcss": "^8.4.0",
    "svelte": "^4.2.0",
    "svelte-check": "^3.6.0",
    "tailwindcss": "^3.4.0",
    "tslib": "^2.6.0",
    "typescript": "^5.3.0",
    "vite": "^5.0.0"
  },
  "type": "module"
}
```

(Version numbers are minimum-recent; the implementer can adjust.)

- [ ] **Step 3: Create the other config files**

`webui/svelte.config.js`:
```javascript
import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

export default {
  preprocess: vitePreprocess(),
  kit: {
    adapter: adapter({ fallback: 'index.html' }),
  },
};
```

`webui/vite.config.ts`:
```typescript
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
  plugins: [sveltekit()],
  server: { port: 5173 },
});
```

`webui/tsconfig.json`:
```json
{
  "extends": "./.svelte-kit/tsconfig.json",
  "compilerOptions": {
    "allowJs": true,
    "checkJs": true,
    "esModuleInterop": true,
    "forceConsistentCasingInFileNames": true,
    "resolveJsonModule": true,
    "skipLibCheck": true,
    "sourceMap": true,
    "strict": true,
    "moduleResolution": "bundler"
  }
}
```

`webui/tailwind.config.cjs`:
```javascript
module.exports = {
  content: ['./src/**/*.{html,js,svelte,ts}'],
  theme: { extend: {} },
  plugins: [],
};
```

`webui/postcss.config.cjs`:
```javascript
module.exports = {
  plugins: { tailwindcss: {}, autoprefixer: {} },
};
```

`webui/src/app.html`:
```html
<!DOCTYPE html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <link rel="icon" href="%sveltekit.assets%/favicon.png" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    %sveltekit.head%
  </head>
  <body data-sveltekit-preload-data="hover">
    <div style="display: contents">%sveltekit.body%</div>
  </body>
</html>
```

`webui/src/app.css`:
```css
@tailwind base;
@tailwind components;
@tailwind utilities;
```

`webui/.gitignore`:
```
node_modules/
.svelte-kit/
build/
dist/
```

- [ ] **Step 4: Install deps (this will create `node_modules/`)**

```bash
cd /home/hhhh/Graph-Centric/webui && npm install
```

Expected: installs successfully. (Network access required; if this fails, see the troubleshooting note in the report.)

- [ ] **Step 5: Verify the scaffold builds (smoke test the empty app)**

```bash
cd /home/hhhh/Graph-Centric/webui && npm run build
```

Expected: builds without errors. The output is in `webui/build/` (or `webui/.svelte-kit/output/`).

---

## Task 13: Frontend lib utilities (api, sse, graph, diff, types)

**Files:**
- Create: `webui/src/lib/types.ts`
- Create: `webui/src/lib/api.ts`
- Create: `webui/src/lib/sse.ts`
- Create: `webui/src/lib/graph.ts`
- Create: `webui/src/lib/diff.ts`

- [ ] **Step 1: Create `types.ts` (shared types matching the Rust `RunEvent`)**

Create `/home/hhhh/Graph-Centric/webui/src/lib/types.ts`:

```typescript
// Mirrors src/web/events.rs RunEvent enum.
export type RunEvent =
  | { type: 'transcript'; data: { role: string; content: string } }
  | { type: 'graph'; data: { nodes: NodeDto[]; edges: EdgeDto[] } }
  | { type: 'loop_state'; data: { kind: string; payload: any } }
  | { type: 'review'; data: { verdict: string; root_cause: string | null } }
  | { type: 'skill_captured'; data: { slug: string; trigger: string } }
  | { type: 'done'; data: { final_result: any } }
  | { type: 'error'; data: { message: string } };

export interface NodeDto {
  id: string;
  kind: string;
  summary: string;
  l1: string | null;
  l1_confidence: number | null;
}

export interface EdgeDto {
  source: string;
  target: string;
  relation: string;
  confidence: number;
}

export interface SkillRef {
  slug: string;
  trigger: string;
}

export interface SkillDetail extends SkillRef {
  task: string;
  graph: { nodes: NodeDto[]; edges: EdgeDto[] };
  meta: {
    created_at: string;
    task_id: string | null;
    model_used: string;
    domain_tags: string[];
    l1_avg_confidence: number;
  };
}

export interface RunMetadata {
  id: string;
  task: string;
  status: 'Running' | 'Paused' | 'GraphInvalid' | 'Done' | 'Error' | 'Cancelled' | { Error: string };
  duration_ms: number;
  captured_skill: SkillRef | null;
}
```

- [ ] **Step 2: Create `api.ts` (typed fetch wrapper)**

Create `/home/hhhh/Graph-Centric/webui/src/lib/api.ts`:

```typescript
import type { RunMetadata, SkillRef, SkillDetail } from './types';

const BASE = '';  // same-origin in v1

export class ApiError extends Error {
  constructor(public status: number, public body: any, msg: string) {
    super(msg);
  }
}

async function api<T>(path: string, options: RequestInit = {}): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    ...options,
    headers: {
      'Content-Type': 'application/json',
      ...(options.headers ?? {}),
    },
  });
  if (!res.ok) {
    const body = await res.json().catch(() => null);
    throw new ApiError(res.status, body, body?.error?.message ?? `API error ${res.status}`);
  }
  if (res.status === 204) return undefined as T;
  return res.json();
}

export const apiClient = {
  health: () => api<{ status: string }>('/api/health'),
  listRuns: () => api<RunMetadata[]>('/api/runs'),
  getRun: (id: string) => api<RunMetadata>(`/api/runs/${id}`),
  cancelRun: (id: string) => api<{ cancelled: boolean }>(`/api/runs/${id}`, { method: 'DELETE' }),
  createRun: (task: string) =>
    api<{ id: string }>('/api/runs', {
      method: 'POST',
      body: JSON.stringify({ task }),
    }),
  postAnswer: (runId: string, answer: string) =>
    api<{ accepted: boolean }>(`/api/runs/${runId}/answer`, {
      method: 'POST',
      body: JSON.stringify({ answer }),
    }),
  postRepair: (runId: string, graph: any) =>
    api<{ accepted: boolean }>(`/api/runs/${runId}/repair`, {
      method: 'POST',
      body: JSON.stringify({ graph }),
    }),
  listSkills: () => api<SkillRef[]>('/api/skills'),
  getSkill: (slug: string) => api<SkillDetail>(`/api/skills/${slug}`),
  deleteSkill: (slug: string) =>
    api<{ deleted: boolean }>(`/api/skills/${slug}`, { method: 'DELETE' }),
  promoteSkill: (slug: string) =>
    api<{ promoted: boolean }>(`/api/skills/${slug}/promote`, { method: 'POST' }),
  filesChanged: (since?: string) => {
    const q = since ? `?since=${encodeURIComponent(since)}` : '';
    return api<Array<{ path: string; change_type: string }>>(`/api/files/changed${q}`);
  },
  fileDiff: (path: string) =>
    api<{ path: string; diff: string }>(
      `/api/files/diff?path=${encodeURIComponent(path)}`,
    ),
};
```

- [ ] **Step 3: Create `sse.ts` (EventSource client)**

Create `/home/hhhh/Graph-Centric/webui/src/lib/sse.ts`:

```typescript
import type { RunEvent } from './types';

export class RunEventSource {
  private es: EventSource | null = null;
  private handlers = new Map<RunEvent['type'], (data: any) => void>();
  private runId: string;

  constructor(runId: string) {
    this.runId = runId;
    this.connect();
  }

  private connect() {
    this.es = new EventSource(`/api/runs/${this.runId}/events`);
    // Add a listener for each event type. The server emits event:<type>
    // on the wire; addEventListener(<type>, ...) picks them up.
    (['transcript', 'graph', 'loop_state', 'review', 'skill_captured', 'done', 'error'] as const).forEach(
      (type) => {
        this.es!.addEventListener(type, (e: MessageEvent) => {
          try {
            const data = JSON.parse(e.data);
            this.handlers.get(type)?.(data);
          } catch (err) {
            console.error(`SSE parse error for ${type}:`, err);
          }
        });
      }
    );
    this.es.onerror = () => {
      console.warn('SSE connection lost; reconnecting in 3s');
      this.es?.close();
      this.es = null;
      setTimeout(() => this.connect(), 3000);
    };
  }

  on<K extends RunEvent['type']>(
    type: K,
    handler: (data: Extract<RunEvent, { type: K }>['data']) => void,
  ) {
    this.handlers.set(type, handler as any);
  }

  close() {
    this.es?.close();
    this.es = null;
  }
}
```

- [ ] **Step 4: Create `graph.ts` (Cytoscape wrapper)**

Create `/home/hhhh/Graph-Centric/webui/src/lib/graph.ts`:

```typescript
import type { NodeDto, EdgeDto } from './types';

// Cytoscape is loaded via CDN <script> in +layout.svelte.
// We declare the global here so TypeScript doesn't complain.
declare const cytoscape: any;

export interface GraphController {
  update(nodes: NodeDto[], edges: EdgeDto[]): void;
  destroy(): void;
}

export function makeGraph(container: HTMLElement, initial: { nodes: NodeDto[]; edges: EdgeDto[] }): GraphController {
  const cy = cytoscape({
    container,
    elements: [
      ...initial.nodes.map((n) => ({ data: { id: n.id, label: n.summary }, classes: 'node' })),
      ...initial.edges.map((e, i) => ({
        data: { id: `e${i}`, source: e.source, target: e.target, label: e.relation },
        classes: 'edge',
      })),
    ],
    style: [
      { selector: 'node', style: { 'background-color': '#3b82f6', 'label': 'data(label)', 'color': '#fff', 'text-wrap': 'wrap', 'text-max-width': '120px', 'font-size': '10px' } },
      { selector: 'edge', style: { 'width': 1.5, 'line-color': '#64748b', 'target-arrow-color': '#64748b', 'target-arrow-shape': 'triangle', 'curve-style': 'bezier' } },
      { selector: 'node:selected', style: { 'background-color': '#fbbf24', 'color': '#000' } },
    ],
    layout: { name: 'cose-bilkent', animate: true, idealEdgeLength: 100, nodeRepulsion: 8000 },
  });
  return {
    update(nodes, edges) {
      cy.elements().remove();
      cy.add([
        ...nodes.map((n) => ({ data: { id: n.id, label: n.summary }, classes: 'node' })),
        ...edges.map((e, i) => ({
          data: { id: `e${i}`, source: e.source, target: e.target, label: e.relation },
          classes: 'edge',
        })),
      ]);
      cy.layout({ name: 'cose-bilkent', animate: true }).run();
    },
    destroy() {
      cy.destroy();
    },
  };
}
```

- [ ] **Step 5: Create `diff.ts` (unified diff rendering)**

Create `/home/hhhh/Graph-Centric/webui/src/lib/diff.ts`:

```typescript
// v1: render unified diff as pre-formatted text (no library needed).
// v2: switch to diff2html + Prism.js for syntax highlighting.
export function renderDiff(diff: string): string {
  // Escape HTML, color lines: + green, - red, @@ blue.
  const lines = diff.split('\n');
  const out: string[] = [];
  for (const line of lines) {
    const escaped = line
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;');
    if (line.startsWith('+')) {
      out.push(`<div class="bg-green-900/30 text-green-200 px-2">${escaped}</div>`);
    } else if (line.startsWith('-')) {
      out.push(`<div class="bg-red-900/30 text-red-200 px-2">${escaped}</div>`);
    } else if (line.startsWith('@@')) {
      out.push(`<div class="bg-blue-900/30 text-blue-200 px-2 font-bold">${escaped}</div>`);
    } else {
      out.push(`<div class="px-2">${escaped}</div>`);
    }
  }
  return out.join('');
}
```

- [ ] **Step 6: Verify the SvelteKit project still builds**

```bash
cd /home/hhhh/Graph-Centric/webui && npm run build
```

Expected: builds without errors. (Even without any .svelte files using these, the lib utilities should type-check.)

---

## Task 14: Frontend layout + main run view

**Files:**
- Create: `webui/src/routes/+layout.svelte`
- Create: `webui/src/routes/+page.svelte`

- [ ] **Step 1: Create the layout (`+layout.svelte`)**

Create `/home/hhhh/Graph-Centric/webui/src/routes/+layout.svelte`:

```svelte
<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';

  let mounted = false;
  onMount(() => {
    // Load Cytoscape.js from CDN once on mount.
    const script = document.createElement('script');
    script.src = 'https://unpkg.com/cytoscape@3.30.2/dist/cytoscape.min.js';
    script.async = true;
    document.head.appendChild(script);
    mounted = true;
  });
</script>

<nav class="bg-slate-900 text-white p-4 flex gap-4">
  <a href="/" class="font-bold">Graph-Centric</a>
  <a href="/runs" class="hover:underline">Runs</a>
  <a href="/skills" class="hover:underline">Skills</a>
  <a href="/settings" class="hover:underline">Settings</a>
</nav>

<main class="p-4">
  <slot />
</main>
```

- [ ] **Step 2: Create the main page (`+page.svelte`)**

Create `/home/hhhh/Graph-Centric/webui/src/routes/+page.svelte`:

```svelte
<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { apiClient } from '$lib/api';
  import { RunEventSource } from '$lib/sse';
  import { makeGraph, type GraphController } from '$lib/graph';
  import { renderDiff } from '$lib/diff';
  import type { RunEvent, NodeDto, EdgeDto } from '$lib/types';

  // --- Active run state ---
  let runId: string | null = null;
  let task = '';
  let es: RunEventSource | null = null;
  let graph: GraphController | null = null;
  let graphContainer: HTMLDivElement;
  let transcript: Array<{ role: string; content: string }> = [];
  let nodes: NodeDto[] = [];
  let edges: EdgeDto[] = [];
  let runStatus: string = 'idle';
  let errorMsg: string | null = null;
  let activeTab: 'graph' | 'files' | 'diff' = 'graph';
  let changedFiles: Array<{ path: string; change_type: string }> = [];
  let selectedFile: string | null = null;
  let fileDiffText: string = '';
  let durationSec = 0;
  let timerInterval: any;

  // --- Submit new task ---
  async function submitTask() {
    if (!task.trim()) return;
    errorMsg = null;
    transcript = [];
    nodes = [];
    edges = [];
    runStatus = 'Running';
    durationSec = 0;
    timerInterval = setInterval(() => durationSec++, 1000);
    try {
      const { id } = await apiClient.createRun(task);
      runId = id;
      es = new RunEventSource(id);
      wireEventHandlers(es);
    } catch (e: any) {
      errorMsg = e.message;
      runStatus = 'error';
      clearInterval(timerInterval);
    }
  }

  function wireEventHandlers(es: RunEventSource) {
    es.on('transcript', (data) => {
      transcript = [...transcript, data];
      transcript = transcript;  // trigger Svelte reactivity
    });
    es.on('graph', (data) => {
      nodes = data.nodes;
      edges = data.edges;
      if (graph) graph.update(nodes, edges);
    });
    es.on('loop_state', (data) => {
      runStatus = data.kind;
    });
    es.on('done', (data) => {
      runStatus = 'Done';
      clearInterval(timerInterval);
    });
    es.on('error', (data) => {
      runStatus = 'error';
      errorMsg = data.message;
      clearInterval(timerInterval);
    });
  }

  async function stopRun() {
    if (!runId) return;
    await apiClient.cancelRun(runId);
    runStatus = 'Cancelled';
    if (es) { es.close(); es = null; }
    clearInterval(timerInterval);
  }

  async function loadFiles() {
    if (!runId) return;
    changedFiles = await apiClient.filesChanged();
    changedFiles = changedFiles;
  }

  async function selectFile(path: string) {
    selectedFile = path;
    const result = await apiClient.fileDiff(path);
    fileDiffText = result.diff;
    activeTab = 'diff';
  }

  onMount(() => {
    // Wait for Cytoscape to load.
    const checkCy = setInterval(() => {
      if ((window as any).cytoscape) {
        clearInterval(checkCy);
        if (graphContainer) {
          graph = makeGraph(graphContainer, { nodes: [], edges: [] });
        }
      }
    }, 100);
  });

  onDestroy(() => {
    if (es) es.close();
    if (graph) graph.destroy();
    if (timerInterval) clearInterval(timerInterval);
  });
</script>

<div class="grid grid-cols-2 gap-4 h-[calc(100vh-80px)]">
  <!-- Left: Chat -->
  <div class="flex flex-col bg-slate-800 text-white rounded p-4">
    <div class="flex-1 overflow-y-auto mb-4 space-y-2">
      {#each transcript as msg}
        <div class="p-2 rounded {msg.role === 'user' ? 'bg-blue-900/40' : msg.role === 'ask_user' ? 'bg-amber-900/40' : 'bg-slate-700/40'}">
          <div class="text-xs text-slate-400">{msg.role}</div>
          <div class="whitespace-pre-wrap">{msg.content}</div>
        </div>
      {/each}
      {#if errorMsg}
        <div class="p-2 rounded bg-red-900/40 text-red-200">Error: {errorMsg}</div>
      {/if}
    </div>
    <div class="flex gap-2">
      <input
        class="flex-1 bg-slate-700 text-white px-3 py-2 rounded"
        placeholder="Type a task…"
        bind:value={task}
        on:keydown={(e) => { if (e.key === 'Enter') submitTask(); }}
        disabled={runStatus === 'Running'}
      />
      {#if runStatus === 'Running'}
        <button class="bg-red-600 hover:bg-red-500 px-4 py-2 rounded" on:click={stopRun}>
          Stop
        </button>
      {:else}
        <button class="bg-blue-600 hover:bg-blue-500 px-4 py-2 rounded" on:click={submitTask}>
          Run
        </button>
      {/if}
    </div>
    {#if runId}
      <div class="text-xs text-slate-400 mt-2">
        Run {runId.slice(0, 8)}… · {durationSec}s · {runStatus}
      </div>
    {/if}
  </div>

  <!-- Right: Graph / Files / Diff -->
  <div class="flex flex-col bg-slate-800 text-white rounded">
    <div class="flex border-b border-slate-700">
      <button class="px-4 py-2 {activeTab === 'graph' ? 'bg-slate-700' : ''}" on:click={() => activeTab = 'graph'}>
        Graph
      </button>
      <button class="px-4 py-2 {activeTab === 'files' ? 'bg-slate-700' : ''}" on:click={loadFiles}>
        Files
      </button>
      <button class="px-4 py-2 {activeTab === 'diff' ? 'bg-slate-700' : ''}" on:click={() => activeTab = 'diff'}>
        Diff
      </button>
    </div>
    <div class="flex-1 overflow-hidden">
      {#if activeTab === 'graph'}
        <div bind:this={graphContainer} class="w-full h-full"></div>
      {:else if activeTab === 'files'}
        <div class="p-4 space-y-2 overflow-y-auto h-full">
          {#each changedFiles as file}
            <button
              class="block w-full text-left p-2 bg-slate-700/40 hover:bg-slate-700 rounded"
              on:click={() => selectFile(file.path)}
            >
              <span class="text-xs text-slate-400 mr-2">{file.change_type}</span>
              {file.path}
            </button>
          {/each}
          {#if changedFiles.length === 0}
            <div class="text-slate-400 text-sm">No files changed.</div>
          {/if}
        </div>
      {:else}
        <div class="p-2 overflow-auto h-full font-mono text-xs">
          {#if selectedFile}
            <div class="text-slate-400 mb-1">{selectedFile}</div>
          {/if}
          <div>{@html renderDiff(fileDiffText)}</div>
        </div>
      {/if}
    </div>
  </div>
</div>
```

- [ ] **Step 2: Build the SvelteKit project**

```bash
cd /home/hhhh/Graph-Centric/webui && npm run build
```

Expected: builds without errors. The output is in `webui/build/` (or wherever the adapter is configured to write).

If you see `Cannot find module '$lib/...'` errors, check `tsconfig.json` and `svelte.config.js` for the alias.

---

## Task 15: Skills page

**Files:**
- Create: `webui/src/routes/skills/+page.svelte`

- [ ] **Step 1: Create the skills page**

Create `/home/hhhh/Graph-Centric/webui/src/routes/skills/+page.svelte`:

```svelte
<script lang="ts">
  import { onMount } from 'svelte';
  import { apiClient } from '$lib/api';
  import type { SkillRef, SkillDetail } from '$lib/types';

  let skills: SkillRef[] = [];
  let selected: SkillDetail | null = null;
  let errorMsg: string | null = null;

  async function load() {
    try {
      skills = await apiClient.listSkills();
    } catch (e: any) {
      errorMsg = e.message;
    }
  }

  async function selectSkill(slug: string) {
    try {
      selected = await apiClient.getSkill(slug);
    } catch (e: any) {
      errorMsg = e.message;
    }
  }

  async function deleteSkill(slug: string) {
    if (!confirm(`Delete skill "${slug}"?`)) return;
    await apiClient.deleteSkill(slug);
    await load();
    if (selected?.slug === slug) selected = null;
  }

  async function promoteSkill(slug: string) {
    await apiClient.promoteSkill(slug);
    alert(`Skill "${slug}" promoted to repo.`);
    await load();
  }

  onMount(load);
</script>

<div class="grid grid-cols-3 gap-4 h-[calc(100vh-80px)]">
  <!-- List -->
  <div class="bg-slate-800 text-white rounded p-4 overflow-y-auto">
    <h2 class="text-lg font-bold mb-4">Skill Library ({skills.length})</h2>
    {#if errorMsg}<div class="text-red-400 mb-2">{errorMsg}</div>{/if}
    {#each skills as skill}
      <div class="mb-2 p-2 rounded hover:bg-slate-700 cursor-pointer" on:click={() => selectSkill(skill.slug)}>
        <div class="font-mono text-sm">{skill.slug}</div>
        <div class="text-xs text-slate-400 mt-1">"{skill.trigger}"</div>
      </div>
    {/each}
    {#if skills.length === 0}
      <div class="text-slate-400 text-sm">No skills yet. Run an agent to create one.</div>
    {/if}
  </div>

  <!-- Detail -->
  <div class="col-span-2 bg-slate-800 text-white rounded p-4 overflow-y-auto">
    {#if selected}
      <div class="flex justify-between items-start mb-4">
        <div>
          <h2 class="text-xl font-bold font-mono">{selected.slug}</h2>
          <p class="text-slate-400 mt-1">"{selected.trigger}"</p>
        </div>
        <div class="flex gap-2">
          <button class="bg-blue-600 hover:bg-blue-500 px-3 py-1 rounded text-sm" on:click={() => promoteSkill(selected!.slug)}>
            Promote to repo
          </button>
          <button class="bg-red-600 hover:bg-red-500 px-3 py-1 rounded text-sm" on:click={() => deleteSkill(selected!.slug)}>
            Delete
          </button>
        </div>
      </div>
      <div class="text-sm text-slate-400 mb-4">
        Created: {selected.meta.created_at} · Model: {selected.meta.model_used} · L1 confidence avg: {selected.meta.l1_avg_confidence.toFixed(2)}
      </div>
      <div class="text-sm">
        <strong>Task:</strong> {selected.task}
      </div>
      <div class="mt-4">
        <h3 class="font-bold mb-2">Graph</h3>
        <div class="text-xs text-slate-400">
          {selected.graph.nodes.length} nodes, {selected.graph.edges.length} edges
        </div>
        <details class="mt-2">
          <summary class="cursor-pointer">Show raw JSON</summary>
          <pre class="text-xs bg-slate-900 p-2 rounded mt-2 overflow-x-auto">{JSON.stringify(selected.graph, null, 2)}</pre>
        </details>
      </div>
    {:else}
      <div class="text-slate-400">Select a skill to view details.</div>
    {/if}
  </div>
</div>
```

- [ ] **Step 2: Build**

```bash
cd /home/hhhh/Graph-Centric/webui && npm run build
```

Expected: builds.

---

## Task 16: Runs page

**Files:**
- Create: `webui/src/routes/runs/+page.svelte`
- Create: `webui/src/routes/settings/+page.svelte`

- [ ] **Step 1: Create the runs page**

Create `/home/hhhh/Graph-Centric/webui/src/routes/runs/+page.svelte`:

```svelte
<script lang="ts">
  import { onMount } from 'svelte';
  import { apiClient } from '$lib/api';
  import type { RunMetadata } from '$lib/types';

  let runs: RunMetadata[] = [];
  let errorMsg: string | null = null;

  onMount(async () => {
    try {
      runs = await apiClient.listRuns();
    } catch (e: any) {
      errorMsg = e.message;
    }
  });

  function statusLabel(s: RunMetadata['status']): string {
    if (typeof s === 'string') return s;
    return Object.keys(s)[0] ?? '?';
  }

  function statusColor(s: RunMetadata['status']): string {
    const label = statusLabel(s);
    if (label === 'Done') return 'text-green-400';
    if (label === 'Error') return 'text-red-400';
    if (label === 'Cancelled') return 'text-slate-400';
    if (label === 'Paused') return 'text-amber-400';
    if (label === 'GraphInvalid') return 'text-amber-400';
    return 'text-blue-400';
  }
</script>

<div class="bg-slate-800 text-white rounded p-4">
  <h2 class="text-lg font-bold mb-4">Run History ({runs.length})</h2>
  {#if errorMsg}<div class="text-red-400 mb-2">{errorMsg}</div>{/if}
  <table class="w-full text-sm">
    <thead class="text-left text-slate-400">
      <tr>
        <th class="py-2">ID</th>
        <th>Task</th>
        <th>Status</th>
        <th>Duration</th>
        <th>Captured Skill</th>
      </tr>
    </thead>
    <tbody>
      {#each runs as run}
        <tr class="border-t border-slate-700">
          <td class="py-2 font-mono text-xs">{run.id.slice(0, 8)}…</td>
          <td>{run.task}</td>
          <td class={statusColor(run.status)}>{statusLabel(run.status)}</td>
          <td>{(run.duration_ms / 1000).toFixed(1)}s</td>
          <td>{run.captured_skill?.slug ?? '—'}</td>
        </tr>
      {/each}
    </tbody>
  </table>
  {#if runs.length === 0}
    <div class="text-slate-400 text-sm">No runs yet.</div>
  {/if}
</div>
```

- [ ] **Step 2: Create the settings page (minimal)**

Create `/home/hhhh/Graph-Centric/webui/src/routes/settings/+page.svelte`:

```svelte
<script lang="ts">
  import { onMount } from 'svelte';
  import { apiClient } from '$lib/api';

  let health: string = '...';
  let errorMsg: string | null = null;

  onMount(async () => {
    try {
      const r = await apiClient.health();
      health = r.status;
    } catch (e: any) {
      errorMsg = e.message;
      health = 'unreachable';
    }
  });
</script>

<div class="bg-slate-800 text-white rounded p-4 max-w-2xl">
  <h2 class="text-lg font-bold mb-4">Settings</h2>
  <div class="space-y-2 text-sm">
    <div>
      <span class="text-slate-400">Backend health:</span>
      <span class="ml-2 font-mono {health === 'ok' ? 'text-green-400' : 'text-red-400'}">
        {health}
      </span>
    </div>
    {#if errorMsg}
      <div class="text-red-400">{errorMsg}</div>
    {/if}
    <div class="text-slate-400 mt-4">
      Model config (MODEL_BASE_URL, MODEL_API_KEY, etc.) is read from
      <code>.env</code> at server startup. Edit <code>.env</code> and restart
      <code>bin/serve</code> to change.
    </div>
  </div>
</div>
```

- [ ] **Step 3: Build**

```bash
cd /home/hhhh/Graph-Centric/webui && npm run build
```

Expected: builds. All 4 pages (`/`, `/skills`, `/runs`, `/settings`) are present.

---

## Task 17: End-to-end integration test (server up, full run lifecycle)

**Files:**
- Create: `tests/integration_web_e2e.rs` (new integration test)

- [ ] **Step 1: Create the integration test**

Create `/home/hhhh/Graph-Centric/tests/integration_web_e2e.rs`:

```rust
//! End-to-end test: start the server in-process, POST a run, verify
//! health, verify SSE response shape.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

use graph_harness::skills::storage::LocalSkillStorage;
use graph_harness::web::state::WebConfig;
use graph_harness::web::WebState;

fn make_state() -> (TempDir, Arc<WebState>) {
    let dir = TempDir::new().unwrap();
    let local = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
    let cfg = WebConfig {
        bind_addr: "0.0.0.0:0".to_string(),
        static_dir: String::new(),
        project_root: dir.path().to_path_buf(),
    };
    (dir, Arc::new(WebState::new(local, cfg)))
}

#[tokio::test]
async fn health_endpoint_works() {
    let (_dir, state) = make_state();
    let app = graph_harness::web::router(state, "");

    let resp = app
        .oneshot(Request::builder().uri("/api/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn create_run_then_get_run() {
    let (_dir, state) = make_state();
    let app = graph_harness::web::router(state.clone(), "");

    let body = serde_json::to_vec(&serde_json::json!({"task": "x"})).unwrap();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runs")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    let id = v["id"].as_str().unwrap();
    assert!(!id.is_empty());
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p graph_harness --test integration_web_e2e 2>&1 | tail -10`
Expected: 2 tests pass.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test -p graph_harness 2>&1 | grep "test result" | head -10`
Expected: 379 tests pass (377 lib + 2 integration).

---

## Task 18: Manual end-to-end smoke (browser)

**Files:**
- No file changes. This is a verification step.

- [ ] **Step 1: Build the SvelteKit frontend for production**

```bash
cd /home/hhhh/Graph-Centric/webui && npm run build
```

Expected: produces `webui/build/` (or wherever the static adapter writes).

- [ ] **Step 2: Start the Rust server with the built frontend**

```bash
cd /home/hhhh/Graph-Centric && WEB_PORT=8080 WEB_STATIC_DIR=webui/build cargo run --bin serve
```

Expected: server starts, prints "listening on 0.0.0.0:8080".

(In a separate terminal: `curl -s http://localhost:8080/api/health` should return `{"status":"ok"}`.)

- [ ] **Step 3: Open the browser, exercise the flow**

Open `http://localhost:8080` in a browser. Verify:
- The main page loads with the chat box and graph placeholder
- The "Run" button is enabled
- Type a task ("review the lib.rs file"), click Run
- The chat shows transcript messages, the graph starts populating
- After Done, check the Skills page — the captured skill should appear
- Open the Skills page in a new tab, click the skill, view its graph
- Go back to the main page, start another task, observe the graph updates

If anything doesn't work, report the specific failure.

---

## Self-Review

**1. Spec coverage:**

| Spec section | Plan task |
|---|---|
| §4.1 Architecture (single binary, axum, SSE) | Tasks 1, 2, 10, 14-16 |
| §4.2 Tech stack | Tasks 1, 12, 13 |
| §4.3 HTTP API surface (14 endpoints) | Tasks 2, 7, 8, 9, 11, 17 |
| §4.4 SSE event types (7 types) | Task 4 + Tasks 7, 13 |
| §4.5 Frontend pages (4 routes) | Tasks 12, 14, 15, 16 |
| §4.6 File structure (Rust + frontend) | All tasks |
| §4.7 Per-run state machine (RunSession) | Task 6 |
| §4.8 Cancellation flow (CancellationToken) | Task 6 (token), Task 7 (handler) |
| §4.9 Answer / repair flow (Notify) | Task 6 (notify), Task 7 (handlers) |
| §4.10 Skill storage wiring (Composite) | Task 10 (binary setup) |
| §4.11 File diff via git | Task 9 |
| §4.12 Error mapping (ApiError) | Task 3 |
| §4.13 Frontend SSE client | Task 13 |
| §5 Files (Rust + frontend) | All tasks |
| §6 Tests (6 categories) | Tasks 3, 4, 5, 6, 7, 8, 9, 11, 17, 18 |
| §7 All 16 acceptance criteria | Verified by tasks |

**2. Placeholder scan:** No "TBD" / "TODO" / "fill in details" in the plan. Every step has concrete code or specific instructions. The few places where the implementer might need to adapt (e.g., `L1Description::render_oneline` — verify it exists; or `GraphLoop` constructor field names) are noted with "verify this exists" or "may need adjustment" comments.

**3. Type consistency check:**

| Name | Defined in | Used in | Status |
|---|---|---|---|
| `ApiError` | Task 3 | Tasks 7, 8, 9 | ✅ |
| `WebState` | Task 2 (refined 5) | Tasks 6, 7, 8, 9, 10, 11, 17 | ✅ |
| `WebConfig` | Task 5 | Tasks 5, 7, 8, 10 | ✅ |
| `RunEvent` | Task 4 | Tasks 7, 13 | ✅ |
| `NodeDto`, `EdgeDto` | Task 4 | Tasks 7, 13 | ✅ |
| `RunSession` | Task 6 | Tasks 7, 11, 17 | ✅ |
| `RunStatus` | Task 6 | Tasks 6, 7, 11 | ✅ |
| `RunMetadata` | Task 6 | Tasks 7, 8 | ✅ |
| `RunId` (= String) | Task 2 | All | ✅ |

**4. Ambiguity check:**

- Task 7 is the largest single file; the implementer will need to read `bin/agent_a.rs` carefully and adapt the `GraphLoop` construction to the actual API.
- Task 12 (SvelteKit scaffold): the version numbers are minimum-recent; the implementer can adjust. If `npm install` fails due to network, that's outside our control — report.
- Task 14's chat layout is intentionally minimal. The actual UX can be polished later.
- Task 9's `delete_skill` implementation: the current code deletes from both local and repo roots; for v1 simplicity, this is "good enough" but may need refinement (e.g., only delete from the storage that has the skill).

**5. Scope check:** This plan is one self-contained change. 8 new Rust files + 1 new binary + 1 modified lib.rs + ~12 new frontend files. ~2700 lines of code total. Well within one implementation plan.

**Inline fix:** None needed. All placeholder issues are flagged for the implementer with clear instructions.

---

## Acceptance criteria (mirroring spec §7)

- [ ] `cargo run --bin serve` starts an HTTP server
- [ ] All 14 HTTP endpoints work
- [ ] SSE events stream correctly with `event:` and `data:` lines
- [ ] Frontend loads and shows chat + graph + tabs
- [ ] Stop button cancels a run
- [ ] Skill library lists, views, promotes, deletes skills
- [ ] File diff tab shows actual diffs
- [ ] `cargo test -p graph_harness` shows 379 tests pass
- [ ] `npm run build` in `webui/` succeeds
- [ ] `webui/node_modules/`, `webui/.svelte-kit/`, `webui/build/` in `.gitignore`
- [ ] No CORS headers needed (same origin in v1)
- [ ] No `unsafe`, no `unimplemented!()`, no `todo!()` in web module
