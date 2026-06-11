//! GET/POST /api/config — runtime engine configuration management.

use super::state::EngineConfig;
use super::errors::ApiError;
use super::WebState;
use axum::{extract::State, Json};
use std::sync::Arc;

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
            if v != "***" && !v.is_empty() {
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
