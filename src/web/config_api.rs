//! GET/POST /api/config — runtime engine configuration management.

use super::state::EngineConfig;
use super::errors::ApiError;
use super::WebState;
use axum::{extract::State, Json};
use std::sync::Arc;

pub async fn get_config(
    State(state): State<Arc<WebState>>,
) -> Json<EngineConfig> {
    let mut cfg = state.config.engine.clone();
    // Mask API key in response.
    if !cfg.model.api_key_masked.is_empty() {
        cfg.model.api_key_masked = mask_key(&cfg.model.api_key_masked);
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
            config.model.api_key_masked = v.to_string();
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
    }

    // Config takes effect on next run. Mask key in response.
    let mut response = config.clone();
    if !response.model.api_key_masked.is_empty() {
        response.model.api_key_masked = mask_key(&response.model.api_key_masked);
    }

    Ok(Json(response))
}

fn mask_key(key: &str) -> String {
    if key.len() <= 8 {
        return "***".to_string();
    }
    format!("{}***{}", &key[..4], &key[key.len() - 4..])
}
