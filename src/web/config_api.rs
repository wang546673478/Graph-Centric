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
        let recent: Vec<serde_json::Value> = hb.recent_rounds.iter().rev().take(10).map(|r| {
            serde_json::json!({
                "round": r.round,
                "outcome": r.outcome.as_str(),
                "run_id": r.run_id,
                "note": r.note,
                "duration_ms": r.duration_ms,
                "at_ms": r.at_ms,
            })
        }).collect();
        Json(serde_json::json!({
            "active": hb.active,
            "prompt": prompt,
            "max_rounds": hb.max_rounds,
            "completed_rounds": hb.completed_rounds,
            "current_run_id": hb.current_run_id,
            "started_at_ms": hb.started_at_ms,
            "outcome_counts": {
                "success": hb.outcome_counts.success,
                "stagnation": hb.outcome_counts.stagnation,
                "cycle": hb.outcome_counts.cycle,
                "sub_task_failed": hb.outcome_counts.sub_task_failed,
                "error": hb.outcome_counts.error,
                "success_rate": hb.outcome_counts.success_rate(),
                "total": hb.outcome_counts.total(),
            },
            "recent_rounds": recent,
            "next_round_hint": hb.next_round_hint(),
        }))
    } else {
        // Inactive: still return the prompt so the settings page can show it.
        let prompt = file_prompt.unwrap_or_default();
        Json(serde_json::json!({
            "active": false,
            "prompt": prompt,
            "max_rounds": 10,
            "completed_rounds": 0,
            "current_run_id": null,
            "started_at_ms": 0,
            "outcome_counts": {
                "success": 0, "stagnation": 0, "cycle": 0,
                "sub_task_failed": 0, "error": 0,
                "success_rate": 0.0, "total": 0,
            },
            "recent_rounds": [],
            "next_round_hint": null,
        }))
    }
}

/// v2 spec §5.5: human-in-the-loop override — inject a hint
/// into the current round's prompt without canceling the loop.
#[derive(Deserialize)]
pub struct InjectHintBody {
    pub hint: String,
}

pub async fn inject_heartbeat_hint(
    State(state): State<Arc<WebState>>,
    Json(body): Json<InjectHintBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut guard = state.heartbeat.lock().await;
    if let Some(ref mut hb) = *guard {
        hb.inject_hint(body.hint);
        Ok(Json(serde_json::json!({"injected": true})))
    } else {
        Err(ApiError::NotFound("no active heartbeat".into()))
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
    cfg.model.advisor_api_key_masked = if cfg.model.advisor_api_key.is_empty() {
        String::new()
    } else {
        mask_key(&cfg.model.advisor_api_key)
    };
    Json(cfg)
}

pub async fn post_config(
    State(state): State<Arc<WebState>>,
    Json(update): Json<serde_json::Value>,
) -> Result<Json<EngineConfig>, ApiError> {
    // Read from disk so the baseline reflects any prior save in this
    // session. (WebState.config is frozen at startup; we re-read here
    // and in api_runs to pick up runtime changes from the Settings UI.)
    let mut config = EngineConfig::load();

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
        // Optional advisor backend (consult_advisor).
        if let Some(v) = model.get("advisor_base_url").and_then(|v| v.as_str()) {
            config.model.advisor_base_url = v.to_string();
        }
        if let Some(v) = model.get("advisor_api_key_masked").and_then(|v| v.as_str()) {
            if !v.is_empty() && !v.contains("***") && v != "***" {
                config.model.advisor_api_key = v.to_string();
            }
        }
        if let Some(v) = model.get("advisor_model").and_then(|v| v.as_str()) {
            config.model.advisor_model = v.to_string();
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
        if let Some(v) = tuning.get("max_repair_rounds").and_then(|v| v.as_u64()) {
            config.loop_tuning.max_repair_rounds = v as usize;
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
        if let Some(v) = tuning.get("auto_apply_skills").and_then(|v| v.as_bool()) {
            config.loop_tuning.auto_apply_skills = v;
        }
        // Stagnation / stuck / tool failure thresholds
        if let Some(v) = tuning.get("stagnation_soft_hint").and_then(|v| v.as_u64()) {
            config.loop_tuning.stagnation_soft_hint = v as u32;
        }
        if let Some(v) = tuning.get("stagnation_hard_hint").and_then(|v| v.as_u64()) {
            config.loop_tuning.stagnation_hard_hint = v as u32;
        }
        if let Some(v) = tuning.get("stagnation_terminate").and_then(|v| v.as_u64()) {
            config.loop_tuning.stagnation_terminate = v as u32;
        }
        if let Some(v) = tuning.get("stuck_soft_hint").and_then(|v| v.as_u64()) {
            config.loop_tuning.stuck_soft_hint = v as u32;
        }
        if let Some(v) = tuning.get("stuck_hard_hint").and_then(|v| v.as_u64()) {
            config.loop_tuning.stuck_hard_hint = v as u32;
        }
        if let Some(v) = tuning.get("stuck_terminate").and_then(|v| v.as_u64()) {
            config.loop_tuning.stuck_terminate = v as u32;
        }
        if let Some(v) = tuning.get("tool_failure_warn_after").and_then(|v| v.as_u64()) {
            config.loop_tuning.tool_failure_warn_after = v as u32;
        }
        if let Some(v) = tuning.get("tool_failure_halt_after").and_then(|v| v.as_u64()) {
            config.loop_tuning.tool_failure_halt_after = v as u32;
        }
        if let Some(v) = tuning.get("event_channel_capacity").and_then(|v| v.as_u64()) {
            config.loop_tuning.event_channel_capacity = v as usize;
        }
        if let Some(v) = tuning.get("force_search_after_filling_stall").and_then(|v| v.as_u64()) {
            config.loop_tuning.force_search_after_filling_stall = v as u32;
        }
        if let Some(v) = tuning.get("convergence_stable_rounds").and_then(|v| v.as_u64()) {
            config.loop_tuning.convergence_stable_rounds = v as u32;
        }
    }

    // Drill-down depth + sub-run timeout
    if let Some(v) = update.get("max_drilldown_depth").and_then(|v| v.as_u64()) {
        config.max_drilldown_depth = v as usize;
    }
    if let Some(v) = update.get("sub_run_timeout_ms").and_then(|v| v.as_u64()) {
        config.sub_run_timeout_ms = v;
    }

    // Policy: deny_patterns, implicit_cwd_verbs
    if let Some(policy) = update.get("policy") {
        if let Some(arr) = policy.get("deny_patterns").and_then(|v| v.as_array()) {
            config.policy.deny_patterns = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
        if let Some(arr) = policy.get("implicit_cwd_verbs").and_then(|v| v.as_array()) {
            config.policy.implicit_cwd_verbs = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
    }

    // AdvancedTuningConfig: 17 fields from src/web/state.rs
    if let Some(adv) = update.get("advanced") {
        if let Some(v) = adv.get("subagent_max_steps").and_then(|v| v.as_u64()) {
            config.advanced.subagent_max_steps = v as usize;
        }
        if let Some(v) = adv.get("explore_max_items_per_step").and_then(|v| v.as_u64()) {
            config.advanced.explore_max_items_per_step = v as usize;
        }
        if let Some(v) = adv.get("explore_max_question_chars").and_then(|v| v.as_u64()) {
            config.advanced.explore_max_question_chars = v as usize;
        }
        if let Some(v) = adv.get("enricher_l2_char_cap").and_then(|v| v.as_u64()) {
            config.advanced.enricher_l2_char_cap = v as usize;
        }
        if let Some(v) = adv.get("enricher_neighbor_limit").and_then(|v| v.as_u64()) {
            config.advanced.enricher_neighbor_limit = v as usize;
        }
        if let Some(v) = adv.get("enricher_l0_only_confidence_cap").and_then(|v| v.as_f64()) {
            config.advanced.enricher_l0_only_confidence_cap = v;
        }
        if let Some(v) = adv.get("skill_match_threshold").and_then(|v| v.as_f64()) {
            config.advanced.skill_match_threshold = v;
        }
        if let Some(v) = adv.get("skill_match_trigger_weight").and_then(|v| v.as_f64()) {
            config.advanced.skill_match_trigger_weight = v;
        }
        if let Some(v) = adv.get("skill_match_slug_weight").and_then(|v| v.as_f64()) {
            config.advanced.skill_match_slug_weight = v;
        }
        if let Some(v) = adv.get("cascade_max_expand_depth").and_then(|v| v.as_u64()) {
            config.advanced.cascade_max_expand_depth = v as usize;
        }
        if let Some(v) = adv.get("validator_default_timeout_ms").and_then(|v| v.as_u64()) {
            config.advanced.validator_default_timeout_ms = v;
        }
        if let Some(v) = adv.get("bash_default_timeout_ms").and_then(|v| v.as_u64()) {
            config.advanced.bash_default_timeout_ms = v;
        }
        if let Some(v) = adv.get("bash_max_timeout_ms").and_then(|v| v.as_u64()) {
            config.advanced.bash_max_timeout_ms = v;
        }
        if let Some(v) = adv.get("proposer_default_max_tokens").and_then(|v| v.as_u64()) {
            config.advanced.proposer_default_max_tokens = v as usize;
        }
        if let Some(v) = adv.get("decomposer_default_max_tokens").and_then(|v| v.as_u64()) {
            config.advanced.decomposer_default_max_tokens = v as usize;
        }
        if let Some(v) = adv.get("subagent_default_max_tokens").and_then(|v| v.as_u64()) {
            config.advanced.subagent_default_max_tokens = v as usize;
        }
        if let Some(v) = adv.get("verifier_l2_excerpt_chars").and_then(|v| v.as_u64()) {
            config.advanced.verifier_l2_excerpt_chars = v as usize;
        }
        if let Some(v) = adv.get("max_auto_repair_cycles").and_then(|v| v.as_u64()) {
            config.advanced.max_auto_repair_cycles = v as usize;
        }
    }

    // Persist to disk so config survives restarts.
    let _ = config.save();
    // Note: `state.config.engine` is a frozen snapshot from startup.
    // It's NOT updated here (would require interior mutability on
    // WebState). api_runs.rs works around this by re-reading from disk
    // at the start of each new run — see `live_engine_config()`.

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
    if !response.model.advisor_api_key.is_empty() {
        response.model.advisor_api_key_masked = mask_key(&response.model.advisor_api_key);
    }

    Ok(Json(response))
}

fn mask_key(key: &str) -> String {
    if key.len() <= 8 { return "***".to_string(); }
    format!("{}***{}", &key[..4], &key[key.len() - 4..])
}
