//! GET/POST /api/config + GET /api/models — runtime configuration + model discovery.

use super::state::EngineConfig;
use super::errors::ApiError;
use super::WebState;
use axum::{extract::{Query, State}, Json};
use serde::Deserialize;
use std::sync::Arc;

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
