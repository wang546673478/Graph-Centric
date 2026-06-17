//! `serve` — the web gateway binary.
//!
//! Wraps the existing agent loop in an axum HTTP server with SSE event
//! streaming. Browse to http://localhost:8080 after starting.
//!
//! Environment:
//!   WEB_PORT          bind port (default 8080)
//!   WEB_STATIC_DIR    path to webui/dist (default "webui/dist")
//!   MODEL_BASE_URL, MODEL_API_KEY, etc.  (from .env or env)

use graph_harness::skills::storage::{LocalSkillStorage, SkillStorage};
use graph_harness::skills::{CompositeSkillStorage, RepoSkillStorage};
use graph_harness::web::state::WebConfig;
use graph_harness::web::WebState;
use std::net::SocketAddr;
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
        .with_target(false)
        .init();

    // Load .env if present.
    let _ = dotenvy::dotenv();

    let config = WebConfig::from_env();
    info!(
        addr = %config.bind_addr,
        static_dir = %config.static_dir,
        "starting web gateway"
    );

    // Build skill storage (composite: local + repo).
    let local_root = LocalSkillStorage::default_install()
        .and_then(|s| s.local_root())
        .unwrap_or_else(|| std::env::temp_dir().join("graph-centric-skills-fallback"));
    let repo_root = config.project_root.join("skills");
    let skill_storage: Arc<dyn graph_harness::skills::SkillStorage> = Arc::new(
        CompositeSkillStorage::new(
            Some(LocalSkillStorage::new(local_root)),
            RepoSkillStorage::new(repo_root),
        ),
    );

    // Build state.
    let state = Arc::new(WebState::new(skill_storage, config.clone()));

    // Sync model config to env vars so new runs pick it up.
    state.config.engine.sync_env();

    // Restore persisted runs from disk so history survives restart.
    state.restore_persisted_runs().await;

    // Heartbeat: auto-resume pending optimization task.
    {
        let mut hb = state.heartbeat.lock().await;
        if let Some(ref mut hb) = *hb {
            if hb.active && hb.current_run_id.is_none() {
                info!(prompt = %hb.prompt, "heartbeat: starting/continuing optimization");
                let id = uuid::Uuid::new_v4().to_string();
                let label = format!("🫀 Round {}/10: {}", hb.completed_rounds + 1, &hb.prompt[..hb.prompt.len().min(80)]);
                let session = Arc::new(graph_harness::web::run_session::RunSession::new(
                    id.clone(),
                    label,
                ));
                state.runs.write().await.insert(id.clone(), session.clone());
                hb.current_run_id = Some(id.clone());
                hb.save();
                let state2 = state.clone();
                let id2 = id.clone();
                tokio::spawn(async move {
                    graph_harness::web::api_runs::drive_run(state2, id2, None, None).await;
                });
            }
        }
    }

    // Build router.
    let app = graph_harness::web::router((*state).clone(), &config.static_dir);

    // Bind.
    let addr: SocketAddr = config.bind_addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(?addr, "listening");

    axum::serve(listener, app).await?;
    Ok(())
}
