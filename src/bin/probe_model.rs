//! `probe-model` — connectivity smoke for whatever backend `.env` points at.
//!
//! Loads `ModelConfig` (which reads `.env` + env vars), sends a one-line
//! prompt to each tier (fast and deep), prints the responses + token
//! usage + latency. Use this before running the full Demo A to confirm
//! the URL, key, and model names actually resolve.
//!
//! ## Usage
//!
//! ```bash
//! # Make sure .env is filled in (see .env.example).
//! cargo run --bin probe-model
//!
//! # Or set vars on the command line:
//! MODEL_BASE_URL=https://api.deepseek.com/v1 \
//!   MODEL_API_KEY=sk-... \
//!   MODEL_NAME_DEFAULT=deepseek-v4-pro \
//!   cargo run --bin probe-model
//! ```
//!
//! Exits 0 on success, 1 on any failure (network, auth, schema).

use graph_harness::model::{Message, Model, ModelConfig, ModelRequest};
use std::time::Instant;
use tracing::{error, info};

#[tokio::main]
async fn main() {
    init_tracing();

    let cfg = match ModelConfig::load() {
        Ok(c) => c,
        Err(e) => {
            error!("config load failed: {e}");
            eprintln!(
                "\nHint: copy `.env.example` to `.env` and fill in MODEL_BASE_URL, MODEL_API_KEY, \
                 and MODEL_NAME_DEFAULT (or MODEL_NAME_FAST + MODEL_NAME_DEEP)."
            );
            std::process::exit(1);
        }
    };

    info!(
        base_url = %cfg.base_url,
        fast = %cfg.fast,
        deep = %cfg.deep,
        has_key = cfg.api_key.is_some(),
        "loaded ModelConfig"
    );

    let fast_ok = probe("fast", cfg.fast_model().as_ref()).await;
    let deep_ok = if cfg.fast == cfg.deep {
        info!("fast and deep tiers are the same model; skipping deep probe");
        true
    } else {
        probe("deep", cfg.deep_model().as_ref()).await
    };

    if fast_ok && deep_ok {
        info!("probe-model: all tiers OK");
    } else {
        error!("probe-model: one or more tiers failed");
        std::process::exit(1);
    }
}

async fn probe(label: &str, model: &dyn Model) -> bool {
    let req = ModelRequest {
        messages: vec![
            Message::system(
                "You are a connectivity probe. Reply with exactly the word \"ok\" and nothing else.",
            ),
            Message::user("ping"),
        ],
        tools: vec![],
        temperature: 0.0,
        // Generous cap — reasoning-style models burn internal tokens before
        // producing visible content; 16-token caps make the response come
        // back empty even though HTTP succeeded.
        max_tokens: Some(256),
        stop: vec![],
    };

    let started = Instant::now();
    match model.complete(req).await {
        Ok(resp) => {
            let dur = started.elapsed();
            info!(
                tier = label,
                model = model.name(),
                latency_ms = dur.as_millis() as u64,
                prompt_tokens = resp.usage.prompt_tokens,
                completion_tokens = resp.usage.completion_tokens,
                total_tokens = resp.usage.total_tokens,
                response = %resp.content.trim(),
                "tier reachable"
            );
            true
        }
        Err(e) => {
            error!(tier = label, model = model.name(), error = %e, "tier unreachable");
            eprintln!(
                "\nHint: check that MODEL_BASE_URL ends in /v1, that MODEL_API_KEY is valid, \
                 and that the backend actually serves `{}`.",
                model.name()
            );
            false
        }
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
}
