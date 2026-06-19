//! GET/POST /api/config + GET /api/models — runtime configuration + model discovery.

use super::state::{EngineConfig, ModelTierConfig};
use super::errors::ApiError;
use super::heartbeat::HeartBeat;
use super::WebState;
use axum::{extract::{Query, State}, Json};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct HeartBeatBody {
    pub prompt: String,
    pub max_rounds: usize,
}

async fn spawn_heartbeat_run(state: &Arc<WebState>, hb: &mut super::heartbeat::HeartBeat) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let label = format!("🫀 Round {}/{}", hb.completed_rounds + 1, hb.max_rounds);
    let prompt = format!(
        "You are working in the project at: {}\n\n{}",
        state.config.project_root.display(),
        hb.prompt
    );
    let session = Arc::new(super::run_session::RunSession::new(
        id.clone(),
        label,
        state.config.engine.loop_tuning.event_channel_capacity,
    ));
    state.runs.write().await.insert(id.clone(), session.clone());
    let initial_transcript = vec![super::api_runs::InitialMessage {
        role: "user".into(),
        content: prompt,
    }];
    hb.current_run_id = Some(id.clone());
    hb.save();
    let state2 = state.clone();
    let id2 = id.clone();
    tokio::spawn(async move {
        super::api_runs::drive_run(state2, id2, None, Some(initial_transcript)).await;
    });
    id
}

pub async fn start_heartbeat(
    State(state): State<Arc<WebState>>,
    Json(body): Json<HeartBeatBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let prompt_path = state.config.project_root.join(".graph_harness_heartbeat_prompt.md");
    let _ = std::fs::write(prompt_path, &body.prompt);
    let mut hb = HeartBeat::start(body.prompt, body.max_rounds);
    let run_id = spawn_heartbeat_run(&state, &mut hb).await;
    let rounds = hb.max_rounds;
    let mut guard = state.heartbeat.lock().await;
    *guard = Some(hb);
    Ok(Json(serde_json::json!({"started": true, "max_rounds": rounds, "run_id": run_id})))
}

pub async fn get_heartbeat(
    State(state): State<Arc<WebState>>,
) -> Json<serde_json::Value> {
    let guard = state.heartbeat.lock().await;
    // Always load the canonical prompt from the .md file if it exists.
    let prompt_path = state.config.project_root.join(".graph_harness_heartbeat_prompt.md");
    let file_prompt = if prompt_path.exists() {
        std::fs::read_to_string(&prompt_path).ok()
    } else {
        None
    };
    if let Some(ref hb) = *guard {
        let prompt = file_prompt.unwrap_or_else(|| hb.prompt.clone());
        Json(serde_json::json!({"active": hb.active, "prompt": prompt, "max_rounds": hb.max_rounds, "completed_rounds": hb.completed_rounds, "current_run_id": hb.current_run_id}))
    } else {
        // Inactive: still return the prompt so the settings page can show it.
        let prompt = file_prompt.unwrap_or_default();
        Json(serde_json::json!({"active": false, "prompt": prompt, "max_rounds": 10, "completed_rounds": 0, "current_run_id": null}))
    }
}

pub async fn start_default_heartbeat(
    State(state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Load prompt from markdown file if it exists, fall back to hardcoded default.
    let prompt_path = state.config.project_root.join(".graph_harness_heartbeat_prompt.md");
    let prompt = if prompt_path.exists() {
        std::fs::read_to_string(&prompt_path).unwrap_or_else(|_| {
            super::heartbeat::DEFAULT_OPTIMIZATION_PROMPT.to_string()
        })
    } else {
        super::heartbeat::DEFAULT_OPTIMIZATION_PROMPT.to_string()
    };
    let mut hb = HeartBeat::start(prompt, 10);
    let run_id = spawn_heartbeat_run(&state, &mut hb).await;
    let mut guard = state.heartbeat.lock().await;
    *guard = Some(hb);
    Ok(Json(serde_json::json!({"started": true, "max_rounds": 10, "run_id": run_id})))
}

#[derive(Deserialize)]
pub struct PromptBody { pub prompt: String }

pub async fn update_heartbeat_prompt(
    State(state): State<Arc<WebState>>,
    Json(body): Json<PromptBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut guard = state.heartbeat.lock().await;
    if let Some(ref mut hb) = *guard {
        hb.prompt = body.prompt;
        let prompt_path = state.config.project_root.join(".graph_harness_heartbeat_prompt.md");
        let _ = std::fs::write(prompt_path, &hb.prompt);
        hb.save();
        Ok(Json(serde_json::json!({"updated": true})))
    } else {
        Err(ApiError::NotFound("no active heartbeat".into()))
    }
}

pub async fn cancel_heartbeat(
    State(state): State<Arc<WebState>>,
) -> Json<serde_json::Value> {
    let mut guard = state.heartbeat.lock().await;
    if let Some(ref mut hb) = *guard { hb.cancel(); }
    Json(serde_json::json!({"cancelled": true}))
}

#[derive(Deserialize)]
pub struct ModelsQuery {
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
}

/// Fetch the list of available models from an OpenAI-compatible endpoint.
/// Tries `{base_url}/models` first, then strips `/v1` and retries for providers
/// (like DeepSeek) that serve models at the root rather than under `/v1`.
pub async fn list_models(
    Query(q): Query<ModelsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| ApiError::Internal(format!("client error: {e}")))?;

    let base = q.base_url.trim_end_matches('/').to_string();

    // Candidate URLs: {base}/models, then {base without /v1}/models
    let mut urls = vec![format!("{base}/models")];
    if let Some(stripped) = base.strip_suffix("/v1").or_else(|| base.strip_suffix("/V1")) {
        urls.push(format!("{stripped}/models"));
    }

    let mut last_err = String::new();
    for url in &urls {
        let mut req = client.get(url);
        if !q.api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", q.api_key));
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.text().await.unwrap_or_default();
                let json: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::json!({
                    "error": "invalid JSON response",
                    "raw": body.chars().take(200).collect::<String>(),
                }));

                let models: Vec<&str> = json
                    .get("data")
                    .and_then(|d| d.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
                            .collect()
                    })
                    .unwrap_or_default();

                return Ok(Json(serde_json::json!({
                    "models": models,
                    "raw": json,
                })));
            }
            Ok(resp) => {
                last_err = format!("HTTP {}", resp.status().as_u16());
            }
            Err(e) => {
                last_err = format!("{e}");
            }
        }
    }

    Err(ApiError::Internal(format!("failed to fetch models from any URL: {last_err}")))
}

pub async fn get_config(
    State(state): State<Arc<WebState>>,
) -> Json<EngineConfig> {
    // Re-read from disk so saved config survives page refresh.
    let mut cfg = EngineConfig::load();
    if !cfg.model.api_key.is_empty() {
        cfg.model.api_key_masked = mask_key(&cfg.model.api_key);
    } else {
        cfg.model.api_key_masked = String::new();
    }
    Json(cfg)
}

pub async fn post_config(
    State(state): State<Arc<WebState>>,
    Json(update): Json<serde_json::Value>,
) -> Result<Json<EngineConfig>, ApiError> {
    let mut config = state.config.engine.clone();

    if let Some(model) = update.get("model") {
        if let Some(v) = model.get("base_url").and_then(|v| v.as_str()) {
            config.model.base_url = v.to_string();
        }
        if let Some(v) = model.get("api_key_masked").and_then(|v| v.as_str()) {
            // Only update if it's a real key, not the masked display form.
            if !v.is_empty() && !v.contains("***") && v != "***" {
                config.model.api_key = v.to_string();
            }
        }
        if let Some(v) = model.get("fast_model").and_then(|v| v.as_str()) {
            config.model.fast_model = v.to_string();
        }
        if let Some(v) = model.get("deep_model").and_then(|v| v.as_str()) {
            config.model.deep_model = v.to_string();
        }
    }
    if let Some(policy) = update.get("policy") {
        if let Some(v) = policy.get("max_concurrent_subagents").and_then(|v| v.as_u64()) {
            config.policy.max_concurrent_subagents = v as usize;
        }
    }
    // Save profiles if sent.
    if let Some(profiles) = update.get("profiles") {
        if let Ok(map) = serde_json::from_value::<std::collections::HashMap<String, ModelTierConfig>>(profiles.clone()) {
            config.profiles = map;
        }
    }
    // Update active_profile AFTER model fields are applied (it only sets the name, not the model).
    if let Some(prof) = update.get("active_profile").and_then(|v| v.as_str()) {
        if !prof.is_empty() {
            config.active_profile = prof.to_string();
        }
    }

    if let Some(tuning) = update.get("loop_tuning") {
        if let Some(v) = tuning.get("max_rounds").and_then(|v| v.as_u64()) {
            config.loop_tuning.max_rounds = v as usize;
        }
        if let Some(v) = tuning.get("cascade_backtrack").and_then(|v| v.as_bool()) {
            config.loop_tuning.cascade_backtrack = v;
        }
        if let Some(v) = tuning.get("thinking_enabled").and_then(|v| v.as_bool()) {
            config.loop_tuning.thinking_enabled = v;
        }
        if let Some(v) = tuning.get("reasoning_effort").and_then(|v| v.as_str()) {
            config.loop_tuning.reasoning_effort = v.to_string();
        }
    }

    // Persist to disk so config survives restarts.
    let _ = config.save();

    // Set env vars so ModelConfig::load() picks them up for subsequent runs.
    // Safety: single-threaded web server, no concurrent access.
    if !config.model.base_url.is_empty() {
        unsafe { std::env::set_var("MODEL_BASE_URL", &config.model.base_url); }
    }
    if !config.model.api_key.is_empty() {
        unsafe { std::env::set_var("MODEL_API_KEY", &config.model.api_key); }
    }
    unsafe {
        std::env::set_var("MODEL_NAME_FAST", &config.model.fast_model);
        std::env::set_var("MODEL_NAME_DEEP", &config.model.deep_model);
    }

    // Mask key in response.
    let mut response = config.clone();
    if !response.model.api_key.is_empty() {
        response.model.api_key_masked = mask_key(&response.model.api_key);
    }

    Ok(Json(response))
}

fn mask_key(key: &str) -> String {
    if key.len() <= 8 { return "***".to_string(); }
    format!("{}***{}", &key[..4], &key[key.len() - 4..])
}
