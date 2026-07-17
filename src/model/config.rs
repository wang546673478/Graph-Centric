//! Model configuration & tiered loading.
//!
//! All model selection comes from environment variables — **never** hardcoded
//! in source. A small `.env` file at the repository root is the recommended
//! place to keep these on a developer machine; `dotenvy` auto-loads it on
//! [`ModelConfig::load`].
//!
//! ## Environment variables
//!
//! | Var                  | Required? | Default                                | Purpose                                          |
//! |----------------------|-----------|----------------------------------------|--------------------------------------------------|
//! | `MODEL_BASE_URL`     | no        | `https://api.minimaxi.com/anthropic`  | Anthropic-protocol endpoint (`/v1/messages`)     |
//! | `MODEL_API_KEY`      | usually   | (empty)                                | API key; falls back to `ANTHROPIC_API_KEY` / `MINIMAX_API_KEY` |
//! | `ANTHROPIC_API_KEY`  | usually   | (empty)                                | Preferred key env var name (per Anthropic convention) |
//! | `MINIMAX_API_KEY`    | no        | (empty)                                | Fallback key env var (MiniMax-specific)          |
//! | `MODEL_NAME_FAST`    | no        | `MiniMax-M3`                           | Fast/cheap model — used by Proposer, Verifier    |
//! | `MODEL_NAME_DEEP`    | no        | `MiniMax-M3`                           | Deeper/slower model — used by Enricher, Repairer |
//! | `MODEL_NAME_DEFAULT` | no        | `MiniMax-M3`                           | Single-model override; if set, both tiers use it |
//!
//! ## Tier rationale
//!
//! Different components have different cost/quality trade-offs:
//!
//! - **Fast tier** (e.g. `MiniMax-M3`): called every turn in the
//!   GraphLoop — Proposer (4 step kinds), Verifier (sampling + self-check).
//!   Volume is high; latency and price dominate.
//! - **Deep tier** (e.g. `MiniMax-M3`): called when correctness matters
//!   more than throughput — L1Enricher (semantic descriptions), LocalRepairer
//!   (must produce a working patch on first try), Reviewer (final judgment).
//!
//! A caller wanting one model everywhere can set `MODEL_NAME_DEFAULT` and
//! both tiers resolve to it.
//!
//! ## Protocol
//!
//! S4 of the OpenAI → Anthropic migration: the factory in this module
//! constructs an `AnthropicModel` (speaking the `/v1/messages` wire
//! protocol with `x-api-key` + `anthropic-version` headers). The
//! OpenAI-compatible client was deleted as part of S6 final cleanup.

use super::anthropic::{AnthropicConfig, AnthropicModel};
use super::Model;
use crate::error::Result;
use std::sync::Arc;

/// Default base URL — MiniMax's Anthropic-compatible endpoint.
const DEFAULT_BASE_URL: &str = "https://api.minimaxi.com/anthropic";
/// Default model — MiniMax-M3 (used for both tiers until S5 introduces a tier split).
const DEFAULT_MODEL: &str = "MiniMax-M3";

/// v2 spec §5.2: which model layer a call belongs to. Used by
/// `model_for_layer` to route the call to the right tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelLayer {
    /// Proposer — the main agent. Cheap / fast tier by default.
    Proposer,
    /// Subagent — dispatched by the proposer. Cheap / fast.
    Subagent,
    /// Verifier L1 sampling — quick drift check. Fast.
    VerifierL1,
    /// Verifier graph self-check — structural pass. Deep.
    VerifierGraph,
    /// Reviewer — final judgment. Deep.
    Reviewer,
    /// Decomposer — turns the world graph into a task DAG. Deep.
    Decomposer,
    /// Cascade backtracker — runs a structured rollback. Deep.
    Cascade,
    /// Unknown / not classified. Defaults to fast.
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub fast: String,
    pub deep: String,
    /// Whether to request chain-of-thought from backends that support a
    /// thinking toggle (DeepSeek reasoning_effort, MiniMax thinking object).
    /// Default true.
    pub thinking_enabled: bool,
    /// DeepSeek-style reasoning effort ("high"/"max"). Ignored by backends
    /// that don't use it.
    pub reasoning_effort: Option<String>,
    /// Optional independent advisor backend: (base_url, api_key, model).
    /// When set, `advisor_model()` returns a client for it so the main
    /// model can `consult_advisor`. Fully independent of the task backend.
    pub advisor: Option<AdvisorConfig>,
}

/// Configuration for an independent advisor backend.
#[derive(Debug, Clone)]
pub struct AdvisorConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
}

impl ModelConfig {
    /// Load `.env` (if present), then read env vars. Errors with a clear
    /// message when a required var is missing.
    ///
    /// `.env` is loaded only on first call within a process; subsequent
    /// calls just read the already-populated environment.
    pub fn load() -> Result<Self> {
        // dotenvy returns Err when no .env exists; that's fine for
        // production-style environments where vars come from the shell.
        let _ = dotenvy::dotenv();
        Self::from_current_env()
    }

    /// Same as [`load`] but does NOT touch `.env`. Useful in tests that
    /// want to inject env vars programmatically.
    ///
    /// Defaults (S4 — OpenAI → Anthropic migration):
    /// - `base_url` → `https://api.minimaxi.com/anthropic` (MiniMax anthropic-compat).
    /// - `api_key` → `MODEL_API_KEY` → `ANTHROPIC_API_KEY` → `MINIMAX_API_KEY` → empty.
    /// - `fast` / `deep` → `MODEL_NAME_FAST` / `MODEL_NAME_DEEP` →
    ///   `MODEL_NAME_DEFAULT` → `MiniMax-M3`.
    ///
    /// None of the env vars are required; everything has a sensible
    /// MiniMax default so a fresh `.env` can run without edits.
    pub fn from_current_env() -> Result<Self> {
        let base_url = std::env::var("MODEL_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        // Read API key with three-way fallback: MODEL_API_KEY (legacy),
        // ANTHROPIC_API_KEY (Anthropic convention), MINIMAX_API_KEY
        // (MiniMax vendor var). First non-empty wins.
        let api_key = std::env::var("MODEL_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty()))
            .or_else(|| std::env::var("MINIMAX_API_KEY").ok().filter(|s| !s.is_empty()));

        // `MODEL_NAME_DEFAULT` overrides both tiers when present.
        let default = std::env::var("MODEL_NAME_DEFAULT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let fast = std::env::var("MODEL_NAME_FAST")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default.clone());
        let deep = std::env::var("MODEL_NAME_DEEP")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default.clone());

        Ok(Self {
            base_url,
            api_key,
            fast,
            deep,
            thinking_enabled: true,
            reasoning_effort: None,
            advisor: Self::advisor_from_env(),
        })
    }

    /// Read the optional advisor backend from `ADVISOR_*` env vars
    /// (with `ANTHROPIC_API_KEY` / `MINIMAX_API_KEY` fallback for the
    /// key). Returns None unless both base_url and model are set.
    fn advisor_from_env() -> Option<AdvisorConfig> {
        let base_url = std::env::var("ADVISOR_BASE_URL").ok().filter(|s| !s.is_empty())?;
        let model = std::env::var("ADVISOR_MODEL").ok().filter(|s| !s.is_empty())?;
        let api_key = std::env::var("ADVISOR_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty()))
            .or_else(|| std::env::var("MINIMAX_API_KEY").ok().filter(|s| !s.is_empty()));
        Some(AdvisorConfig { base_url, api_key, model })
    }

    /// Build from web engine config (JSON file) — bypasses env vars entirely.
    pub fn from_engine_config(cfg: &crate::web::state::ModelTierConfig) -> Self {
        let advisor = if !cfg.advisor_base_url.is_empty() && !cfg.advisor_model.is_empty() {
            Some(AdvisorConfig {
                base_url: cfg.advisor_base_url.clone(),
                api_key: if cfg.advisor_api_key.is_empty() { None } else { Some(cfg.advisor_api_key.clone()) },
                model: cfg.advisor_model.clone(),
            })
        } else {
            None
        };
        Self {
            base_url: cfg.base_url.clone(),
            api_key: if cfg.api_key.is_empty() { None } else { Some(cfg.api_key.clone()) },
            fast: cfg.fast_model.clone(),
            deep: cfg.deep_model.clone(),
            thinking_enabled: true,
            reasoning_effort: None,
            advisor,
        }
    }

    /// Programmatic constructor — useful for tests and embedded callers
    /// that don't want env at all.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        fast: impl Into<String>,
        deep: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key,
            fast: fast.into(),
            deep: deep.into(),
            thinking_enabled: true,
            reasoning_effort: None,
            advisor: None,
        }
    }

    /// Set an advisor backend programmatically.
    pub fn with_advisor(mut self, base_url: impl Into<String>, api_key: Option<String>, model: impl Into<String>) -> Self {
        self.advisor = Some(AdvisorConfig {
            base_url: base_url.into(),
            api_key,
            model: model.into(),
        });
        self
    }

    /// Set thinking behavior (chain-of-thought on/off + DeepSeek effort).
    pub fn with_thinking(mut self, enabled: bool, reasoning_effort: Option<String>) -> Self {
        self.thinking_enabled = enabled;
        self.reasoning_effort = reasoning_effort;
        self
    }

    /// Build the **fast-tier** model client.
    pub fn fast_model(&self) -> Arc<dyn Model> {
        self.build(&self.fast)
    }

    /// Build the **deep-tier** model client.
    pub fn deep_model(&self) -> Arc<dyn Model> {
        self.build(&self.deep)
    }

    /// v2 spec §5.2: pick the right model for a given layer.
    /// Proposer / SubAgent / VerifierL1 → fast (cheap, high throughput).
    /// Reviewer / Decomposer / Cascade / VerifierGraph → deep (slow,
    /// high quality). When the layer is unknown, falls back to
    /// the fast tier to keep the run moving.
    pub fn model_for_layer(&self, layer: ModelLayer) -> Arc<dyn Model> {
        match layer {
            ModelLayer::Proposer => self.fast_model(),
            ModelLayer::Subagent => self.fast_model(),
            ModelLayer::VerifierL1 => self.fast_model(),
            ModelLayer::VerifierGraph => self.deep_model(),
            ModelLayer::Reviewer => self.deep_model(),
            ModelLayer::Decomposer => self.deep_model(),
            ModelLayer::Cascade => self.deep_model(),
            ModelLayer::Unknown => self.fast_model(),
        }
    }

    /// Build the optional **advisor** model client. Returns None when no
    /// advisor backend is configured. The advisor uses its own base_url +
    /// key + model and speaks the same Anthropic-protocol as the main
    /// client (S4 — both client paths now go through `AnthropicModel`).
    pub fn advisor_model(&self) -> Option<Arc<dyn Model>> {
        let a = self.advisor.as_ref()?;
        Some(self.build_anthropic(&a.base_url, a.api_key.as_deref(), &a.model))
    }

    /// Internal: construct an `AnthropicModel` from a (base_url, api_key,
    /// model) tuple. Centralizes the AnthropicConfig wiring so advisor
    /// and tier builders can't drift.
    fn build_anthropic(
        &self,
        base_url: &str,
        api_key: Option<&str>,
        model: &str,
    ) -> Arc<dyn Model> {
        let cfg = AnthropicConfig {
            base_url: base_url.to_string(),
            api_key: api_key.unwrap_or("").to_string(),
            model: model.to_string(),
            ..Default::default()
        };
        Arc::new(AnthropicModel::new(cfg))
    }

    /// Build the configured-tier AnthropicModel client.
    /// Returns `Arc<dyn Model>` so callers (proposer, decomposer, etc.)
    /// keep their existing trait-surface call patterns.
    fn build(&self, name: &str) -> Arc<dyn Model> {
        self.build_anthropic(&self.base_url, self.api_key.as_deref(), name)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env tests must run serially because they manipulate process-wide state.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn scoped<F: FnOnce()>(setup: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap();
        // Snapshot any vars we plan to overwrite so we can restore them.
        let snapshot: Vec<(String, Option<String>)> = setup
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
            .collect();
        for (k, v) in setup {
            match v {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        f();
        // Restore originals.
        for (k, v) in snapshot {
            match v {
                Some(val) => unsafe { std::env::set_var(&k, val) },
                None => unsafe { std::env::remove_var(&k) },
            }
        }
    }

    // -- S4: defaults point at MiniMax anthropic-compat --

    #[test]
    fn missing_base_url_defaults_to_minimax() {
        scoped(
            &[
                ("MODEL_BASE_URL", None),
                ("MODEL_NAME_FAST", Some("x")),
                ("MODEL_NAME_DEEP", Some("y")),
            ],
            || {
                let cfg = ModelConfig::from_current_env().unwrap();
                assert_eq!(cfg.base_url, DEFAULT_BASE_URL);
                assert_eq!(cfg.fast, "x");
                assert_eq!(cfg.deep, "y");
            },
        );
    }

    #[test]
    fn missing_tiers_default_to_minimax_m3() {
        scoped(
            &[
                ("MODEL_BASE_URL", None),
                ("MODEL_NAME_FAST", None),
                ("MODEL_NAME_DEEP", None),
                ("MODEL_NAME_DEFAULT", None),
            ],
            || {
                let cfg = ModelConfig::from_current_env().unwrap();
                assert_eq!(cfg.fast, DEFAULT_MODEL);
                assert_eq!(cfg.deep, DEFAULT_MODEL);
                assert_eq!(cfg.base_url, DEFAULT_BASE_URL);
                assert!(cfg.api_key.is_none());
            },
        );
    }

    #[test]
    fn default_fills_in_missing_tiers() {
        scoped(
            &[
                ("MODEL_BASE_URL", Some("https://x/v1")),
                ("MODEL_NAME_FAST", None),
                ("MODEL_NAME_DEEP", None),
                ("MODEL_NAME_DEFAULT", Some("only-model")),
                ("MODEL_API_KEY", None),
            ],
            || {
                let cfg = ModelConfig::from_current_env().unwrap();
                assert_eq!(cfg.fast, "only-model");
                assert_eq!(cfg.deep, "only-model");
                assert!(cfg.api_key.is_none());
            },
        );
    }

    #[test]
    fn explicit_tiers_win_over_default() {
        scoped(
            &[
                ("MODEL_BASE_URL", Some("https://x/v1")),
                ("MODEL_NAME_FAST", Some("flash")),
                ("MODEL_NAME_DEEP", Some("pro")),
                ("MODEL_NAME_DEFAULT", Some("ignored")),
                ("MODEL_API_KEY", Some("sk-abc")),
            ],
            || {
                let cfg = ModelConfig::from_current_env().unwrap();
                assert_eq!(cfg.fast, "flash");
                assert_eq!(cfg.deep, "pro");
                assert_eq!(cfg.api_key.as_deref(), Some("sk-abc"));
            },
        );
    }

    #[test]
    fn empty_string_treated_as_unset() {
        scoped(
            &[
                ("MODEL_BASE_URL", Some("https://x/v1")),
                ("MODEL_NAME_FAST", Some("")),
                ("MODEL_NAME_DEFAULT", Some("fallback")),
                ("MODEL_NAME_DEEP", Some("d")),
                ("MODEL_API_KEY", Some("")),
            ],
            || {
                let cfg = ModelConfig::from_current_env().unwrap();
                assert_eq!(cfg.fast, "fallback"); // empty string → fallback
                assert!(cfg.api_key.is_none()); // empty key → None
            },
        );
    }

    #[test]
    fn anthropic_api_key_fallback_when_model_api_key_unset() {
        scoped(
            &[
                ("MODEL_BASE_URL", None),
                ("MODEL_API_KEY", None),
                ("ANTHROPIC_API_KEY", Some("sk-ant-test")),
                ("MINIMAX_API_KEY", None),
            ],
            || {
                let cfg = ModelConfig::from_current_env().unwrap();
                assert_eq!(cfg.api_key.as_deref(), Some("sk-ant-test"));
            },
        );
    }

    #[test]
    fn minimax_api_key_fallback_when_others_unset() {
        scoped(
            &[
                ("MODEL_BASE_URL", None),
                ("MODEL_API_KEY", None),
                ("ANTHROPIC_API_KEY", None),
                ("MINIMAX_API_KEY", Some("sk-minimax-test")),
            ],
            || {
                let cfg = ModelConfig::from_current_env().unwrap();
                assert_eq!(cfg.api_key.as_deref(), Some("sk-minimax-test"));
            },
        );
    }

    #[test]
    fn model_api_key_wins_over_anthropic_fallback() {
        scoped(
            &[
                ("MODEL_BASE_URL", None),
                ("MODEL_API_KEY", Some("sk-primary")),
                ("ANTHROPIC_API_KEY", Some("sk-ant-should-lose")),
            ],
            || {
                let cfg = ModelConfig::from_current_env().unwrap();
                // First non-empty wins in declaration order: MODEL_API_KEY
                // takes precedence over ANTHROPIC_API_KEY.
                assert_eq!(cfg.api_key.as_deref(), Some("sk-primary"));
            },
        );
    }

    #[test]
    fn programmatic_constructor_works() {
        let cfg = ModelConfig::new("https://x/v1", Some("sk-test".into()), "a", "b");
        assert_eq!(cfg.fast, "a");
        assert_eq!(cfg.deep, "b");
        let fast = cfg.fast_model();
        assert_eq!(fast.name(), "a");
        let deep = cfg.deep_model();
        assert_eq!(deep.name(), "b");
    }

    #[test]
    fn advisor_model_none_when_unconfigured() {
        let cfg = ModelConfig::new("https://x/v1", None, "a", "b");
        assert!(cfg.advisor_model().is_none());
    }

    #[test]
    fn advisor_model_built_when_configured() {
        let cfg = ModelConfig::new("https://x/v1", None, "a", "b")
            .with_advisor("https://api.deepseek.com/anthropic", Some("sk-adv".into()), "deepseek-v4-pro");
        let advisor = cfg.advisor_model().expect("advisor should be Some");
        // S4: AnthropicModel::name() returns the cfg model (parity with
        // the OpenAI client). The advisor model here is "deepseek-v4-pro".
        assert_eq!(advisor.name(), "deepseek-v4-pro");
    }
}
