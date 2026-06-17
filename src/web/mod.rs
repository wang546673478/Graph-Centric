//! Web gateway: axum HTTP/WS server wrapping the existing agent loop.
//!
//! See `docs/superpowers/specs/2026-06-10-v2-architecture-design.md` for the v2 design.
//!
//! This module is a thin HTTP surface. The actual agent logic lives in
//! `crate::agent` (GraphLoop, SubAgent, etc.) and `crate::skills` (skill
//! storage). The web module exposes these via REST + WebSocket.

pub mod api_files;
pub mod api_runs;
pub mod api_skills;
pub mod checkpoint;
pub mod config_api;
pub mod errors;
pub mod events;
pub mod heartbeat;
pub mod persistence;
pub mod run_session;
pub mod state;
pub mod ws;

use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
use tower_http::services::ServeDir;
use crate::skills::SkillStorage;

/// Shared application state passed to every axum handler.
#[derive(Clone)]
pub struct WebState {
    pub runs: Arc<tokio::sync::RwLock<std::collections::HashMap<RunId, Arc<run_session::RunSession>>>>,
    pub skills: Arc<dyn SkillStorage>,
    pub config: state::WebConfig,
    pub persistence: persistence::RunPersistence,
    pub heartbeat: Arc<tokio::sync::Mutex<Option<heartbeat::HeartBeat>>>,
}

impl WebState {
    pub fn new(skills: Arc<dyn SkillStorage>, config: state::WebConfig) -> Self {
        let persistence = persistence::RunPersistence::new(&config.project_root);
        let hb = heartbeat::HeartBeat::load();
        Self {
            runs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            skills,
            config,
            persistence,
            heartbeat: Arc::new(tokio::sync::Mutex::new(hb)),
        }
    }

    /// Restore completed runs from disk so the history survives restarts.
    pub async fn restore_persisted_runs(&self) {
        match self.persistence.load_all_runs() {
            Ok(metas) => {
                let mut runs = self.runs.write().await;
                for meta in metas {
                    if !runs.contains_key(&meta.id) {
                        let session = Arc::new(run_session::RunSession::new(
                            meta.id.clone(),
                            meta.task.clone(),
                        ));
                        // Override status + duration from persisted metadata.
                        *session.status.write().await = meta.status.clone();
                        *session.persisted_duration_ms.lock().await = meta.duration_ms;
                        *session.tokens_used.lock().await = meta.tokens_used;
                        runs.insert(meta.id.clone(), session);
                    }
                }
                tracing::info!(count = runs.len(), "restored persisted runs");
            }
            Err(e) => tracing::warn!(error = %e, "failed to load persisted runs"),
        }
    }
}

/// Unique identifier for a run (UUID v4 string).
pub type RunId = String;

/// Build the axum Router. `static_dir` is the path to `webui/dist/`
/// (or empty string to skip the static-file mount in tests).
pub fn router(state: WebState, static_dir: &str) -> Router {
    let state = Arc::new(state);
    let api = Router::new()
        .route("/health", get(api_runs::health))
        .route("/usage", get(api_runs::get_usage))
        .route("/config", get(config_api::get_config).post(config_api::post_config))
        .route("/models", get(config_api::list_models))
        .route("/heartbeat", get(config_api::get_heartbeat).post(config_api::start_heartbeat))
        .route("/heartbeat/default", post(config_api::start_default_heartbeat))
        .route("/heartbeat/cancel", post(config_api::cancel_heartbeat))
        .route("/runs", get(api_runs::list_runs).post(api_runs::create_run))
        .route("/runs/:id", get(api_runs::get_run).delete(api_runs::cancel_run))
        .route("/runs/:id/events", get(api_runs::run_events))
        .route("/runs/:id/checkpoints", get(api_runs::list_checkpoints))
        .route("/runs/:id/checkpoints/:idx", get(api_runs::get_checkpoint))
        .route("/runs/:id/branch", post(api_runs::create_branch))
        .route("/runs/:id/answer", post(api_runs::post_answer))
        .route("/runs/:id/repair", post(api_runs::post_repair))
        .route("/skills", get(api_skills::list_skills))
        .route("/skills/:slug", get(api_skills::get_skill).delete(api_skills::delete_skill))
        .route("/skills/:slug/promote", post(api_skills::promote_skill))
        .route("/files/changed", get(api_files::files_changed))
        .route("/files/diff", get(api_files::file_diff))
        .with_state(state.clone());

    let ws_routes = Router::new()
        .route("/ws/runs/:id", get(ws::ws_handler))
        .with_state(state);

    let mut app = Router::new()
        .nest("/api", api)
        .merge(ws_routes);

    if !static_dir.is_empty() {
        app = app.fallback_service(ServeDir::new(static_dir));
    }

    app
}

pub use errors::ApiError;
