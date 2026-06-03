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
use super::{RunId, WebState};
use crate::agent::decomposer::Decomposer;
use crate::agent::dispatcher::Dispatcher;
use crate::agent::enricher::L1Enricher;
use crate::agent::graph_loop::{GraphLoop, GraphLoopConfig, LoopState};
use crate::agent::proposer::GraphProposer;
use crate::agent::repairer::LocalRepairer;
use crate::agent::reviewer::Reviewer;
use crate::agent::subagent::SubAgent;
use crate::agent::validator::{BashCheckValidator, PostExecutionValidator};
use crate::agent::verifier::Verifier;
use crate::graph::Graph;
use crate::model::ModelConfig;
use crate::tools::{BashTool, DangerousCommandDeny, ToolContext, ToolRegistry};
use axum::{
    extract::{Path, State},
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
    // Sort by started_at descending (newest first). duration_ms is
    // monotonically increasing, so it acts as a proxy for start time.
    out.sort_by(|a, b| b.duration_ms.cmp(&a.duration_ms));
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
    let session = runs
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    Ok(Json(session.metadata().await))
}

// --- Cancel run ---

pub async fn cancel_run(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    session.cancel.cancel();
    Ok(Json(serde_json::json!({"cancelled": true})))
}

// --- SSE event stream ---

pub async fn run_events(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    let rx = session.event_tx.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
        .filter_map(|res| res.ok()) // skip lagged/inactive
        .map(|event: RunEvent| {
            let sse_event = Event::default()
                .event(event.event_name())
                .json_data(event)
                .unwrap_or_else(|_| Event::default().comment("serialization error"));
            Ok::<_, Infallible>(sse_event)
        });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
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
    let session = runs
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
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
    let session = runs
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
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
    let cfg = match ModelConfig::load() {
        Ok(c) => c,
        Err(e) => {
            session.emit(RunEvent::Error {
                message: format!("config error: {e}"),
            });
            *session.status.write().await = RunStatus::Error(e.to_string());
            return;
        }
    };
    let fast_model = cfg.fast_model();
    let deep_model = cfg.deep_model();

    // Tools.
    let tool_registry = Arc::new({
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(BashTool::new()));
        reg
    });
    // A tiny ToolContext just to keep `tool_cwd` canonical; the loop
    // also keeps a copy. We don't actually need to call it from here.
    let _tool_ctx = ToolContext::new(state.config.project_root.clone());

    // Proposer (with skills).
    let proposer = GraphProposer::new(
        fast_model.clone(),
        tool_registry.clone(),
        Some(state.skills.clone()),
    );

    // Verifier, enricher, repairer.
    let verifier = Verifier::with_model(fast_model.clone());
    let loader: Arc<dyn crate::context::SourceLoader> =
        Arc::new(crate::context::NullSourceLoader);
    let enricher = L1Enricher::new(deep_model.clone(), loader.clone());
    let repairer = LocalRepairer::new(deep_model.clone()).with_l1_enricher(enricher.clone());

    // Phase 3 — Decomposer + Dispatcher + SubAgent.
    let decomposer = Decomposer::new(deep_model.clone());
    let subagent = Arc::new(
        SubAgent::new(fast_model.clone())
            .with_tools(tool_registry.clone())
            .with_policy(Arc::new(DangerousCommandDeny::new()))
            .with_tool_cwd(state.config.project_root.clone())
            .with_tool_output_cap(6_000)
            .with_max_steps(6),
    );
    let dispatcher = Dispatcher::new(subagent).with_max_concurrent(3);

    // Phase 4 — Reviewer + PostExecutionValidator.
    let reviewer = Reviewer::with_model(deep_model.clone());
    let validator: Arc<dyn PostExecutionValidator> =
        Arc::new(BashCheckValidator::cargo_check_for(&state.config.project_root));

    let loop_cfg = GraphLoopConfig {
        max_rounds: 24,
        max_repair_rounds: 3,
        tool_cwd: state.config.project_root.clone(),
        tool_output_cap: 8_000,
        tool_policy: Arc::new(DangerousCommandDeny::new()),
    };

    let mut gl = GraphLoop::new(
        session.task.clone(),
        proposer,
        verifier,
        Some(repairer),
        tool_registry.clone(),
        loop_cfg,
    )
    .with_l1_enricher(enricher)
    .with_decomposer(decomposer)
    .with_dispatcher(dispatcher)
    .with_subagent_loader(loader)
    .with_reviewer(reviewer)
    .with_validator(validator);

    // Main loop.
    loop {
        if session.cancel.is_cancelled() {
            *session.status.write().await = RunStatus::Cancelled;
            session.emit(RunEvent::Done {
                final_result: serde_json::json!({"status": "cancelled"}),
            });
            return;
        }

        let state_clone = gl.step().await;
        session.emit_graph_snapshot(&gl.graph).await;
        let kind = state_kind(&state_clone);
        session.emit(RunEvent::LoopState {
            kind: kind.to_string(),
            payload: loop_state_payload(&state_clone),
        });

        match state_clone {
            LoopState::Paused { question, .. } => {
                *session.status.write().await = RunStatus::Paused;
                session.emit(RunEvent::Transcript {
                    role: "ask_user".into(),
                    content: question.clone(),
                });
                let answer = session.await_answer().await;
                gl.resume(answer);
            }
            LoopState::GraphInvalid { source, errors, snapshot } => {
                *session.status.write().await = RunStatus::GraphInvalid;
                let payload = serde_json::json!({
                    "source": format!("{source:?}"),
                    "error_count": errors.len(),
                    "snapshot": {
                        "nodes": snapshot.node_count(),
                        "edges": snapshot.edge_count(),
                    }
                });
                session.emit(RunEvent::LoopState {
                    kind: "GraphInvalid".into(),
                    payload,
                });
                // v1: receive a repaired graph from the user (or any graph,
                // really — caller decides) and resume. We don't auto-apply
                // patches here; that's the caller's job.
                let repaired = session.await_repair().await;
                gl.resume_with_repaired_graph(repaired);
            }
            LoopState::Done(final_result) => {
                *session.status.write().await = RunStatus::Done;
                if let Some(skill) = final_skill_ref(&final_result.graph) {
                    *session.captured_skill.write().await = Some(skill);
                }
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
                *session.status.write().await =
                    RunStatus::Error(format!("task failed: {failures:?}"));
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

fn state_kind(s: &LoopState) -> &'static str {
    match s {
        LoopState::Running => "Running",
        LoopState::Paused { .. } => "Paused",
        LoopState::GraphInvalid { .. } => "GraphInvalid",
        LoopState::TaskFailed { .. } => "TaskFailed",
        LoopState::Done(_) => "Done",
        LoopState::Error(_) => "Error",
    }
}

fn loop_state_payload(s: &LoopState) -> serde_json::Value {
    match s {
        LoopState::Running => serde_json::json!({}),
        LoopState::Paused { question, rationale } => {
            serde_json::json!({"question": question, "rationale": rationale})
        }
        LoopState::GraphInvalid { source, errors, snapshot } => serde_json::json!({
            "source": format!("{source:?}"),
            "error_count": errors.len(),
            "snapshot": {
                "nodes": snapshot.node_count(),
                "edges": snapshot.edge_count(),
            }
        }),
        LoopState::TaskFailed { failures } => {
            serde_json::json!({"failure_count": failures.len()})
        }
        LoopState::Done(r) => serde_json::json!({
            "rounds": r.rounds,
            "graph_nodes": r.graph.node_count(),
            "graph_edges": r.graph.edge_count(),
        }),
        LoopState::Error(msg) => serde_json::json!({"message": msg}),
    }
}

fn final_skill_ref(graph: &Graph) -> Option<crate::skills::SkillRef> {
    if graph.node_count() == 0 {
        return None;
    }
    // Synthesize a SkillRef just from the final graph — a real capture
    // would call `capture_skill` which needs a model for slug + trigger.
    // For v1 we hand the UI a stable reference the user can promote
    // to a real skill.
    Some(crate::skills::SkillRef {
        slug: format!("run-{}", &uuid::Uuid::new_v4().to_string()[..8]),
        trigger: format!("auto-captured from run with {} nodes", graph.node_count()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use crate::skills::storage::LocalSkillStorage;

    fn make_state() -> Arc<WebState> {
        let dir = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let cfg = super::super::state::WebConfig {
            bind_addr: "0.0.0.0:0".to_string(),
            static_dir: String::new(),
            project_root: dir.path().to_path_buf(),
        };
        Arc::new(super::super::WebState::new(local, cfg))
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
            Json(CreateRunBody {
                task: "test".into(),
            }),
        )
        .await
        .unwrap()
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn get_unknown_run_returns_404() {
        let state = make_state();
        let err = get_run(State(state), Path("nope".into())).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cancel_unknown_run_returns_404() {
        let state = make_state();
        let err = cancel_run(State(state), Path("nope".into())).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn post_answer_to_running_run_returns_409() {
        let state = make_state();
        let id = uuid::Uuid::new_v4().to_string();
        let session = Arc::new(RunSession::new(id.clone(), "t".into()));
        state.runs.write().await.insert(id.clone(), session);
        let err = post_answer(
            State(state),
            Path(id),
            Json(AnswerBody {
                answer: "x".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::CONFLICT);
    }
}
