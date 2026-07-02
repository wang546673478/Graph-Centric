//! Model configuration & tiered loading.
//!
//! All model selection comes from environment variables — **never** hardcoded
//! in source. A small `.env` file at the repository root is the recommended
//! place to keep these on a developer machine; `dotenvy` auto-loads it on
//! [`ModelConfig::load`].
//!
//! ## Environment variables
//!
//! | Var                  | Required? | Purpose                                          |
//! |----------------------|-----------|--------------------------------------------------|
//! | `MODEL_BASE_URL`     | yes       | OpenAI-compatible endpoint, e.g. `https://api.deepseek.com/v1` |
//! | `MODEL_API_KEY`      | usually   | Bearer token; required for cloud endpoints       |
//! | `MODEL_NAME_FAST`    | yes       | Fast/cheap model — used by Proposer, Verifier    |
//! | `MODEL_NAME_DEEP`    | yes       | Deeper/slower model — used by Enricher, Repairer |
//! | `MODEL_NAME_DEFAULT` | no        | Single-model override; if set, both tiers use it |
//!
//! ## Tier rationale
//!
//! Different components have different cost/quality trade-offs:
//!
//! - **Fast tier** (e.g. `deepseek-v4-flash`): called every turn in the
//!   GraphLoop — Proposer (4 step kinds), Verifier (sampling + self-check).
//!   Volume is high; latency and price dominate.
//! - **Deep tier** (e.g. `deepseek-v4-pro`): called when correctness matters
//!   more than throughput — L1Enricher (semantic descriptions), LocalRepairer
//!   (must produce a working patch on first try), Reviewer (final judgment).
//!
//! A caller wanting one model everywhere can set `MODEL_NAME_DEFAULT` and
//! both tiers resolve to it.

use super::{Model, OpenAICompatModel};
use crate::error::{HarnessError, Result};
use std::sync::Arc;

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
    pub fn from_current_env() -> Result<Self> {
        let base_url = std::env::var("MODEL_BASE_URL")
            .map_err(|_| HarnessError::model("MODEL_BASE_URL not set"))?;
        let api_key = std::env::var("MODEL_API_KEY").ok().filter(|s| !s.is_empty());

        // `MODEL_NAME_DEFAULT` overrides both tiers when present.
        let default = std::env::var("MODEL_NAME_DEFAULT").ok().filter(|s| !s.is_empty());
        let fast = std::env::var("MODEL_NAME_FAST")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| default.clone())
            .ok_or_else(|| {
                HarnessError::model(
                    "MODEL_NAME_FAST not set (and no MODEL_NAME_DEFAULT fallback)",
                )
            })?;
        let deep = std::env::var("MODEL_NAME_DEEP")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| default.clone())
            .ok_or_else(|| {
                HarnessError::model(
                    "MODEL_NAME_DEEP not set (and no MODEL_NAME_DEFAULT fallback)",
                )
            })?;

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

    /// Read the optional advisor backend from `ADVISOR_*` env vars.
    /// Returns None unless both base_url and model are set.
    fn advisor_from_env() -> Option<AdvisorConfig> {
        let base_url = std::env::var("ADVISOR_BASE_URL").ok().filter(|s| !s.is_empty())?;
        let model = std::env::var("ADVISOR_MODEL").ok().filter(|s| !s.is_empty())?;
        let api_key = std::env::var("ADVISOR_API_KEY").ok().filter(|s| !s.is_empty());
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
        Arc::new(self.build(&self.fast))
    }

    /// Build the **deep-tier** model client.
    pub fn deep_model(&self) -> Arc<dyn Model> {
        Arc::new(self.build(&self.deep))
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
    /// key + model, and automatically gets the right per-backend request
    /// format via `ModelCapabilities::from_model_name` (so a MiniMax
    /// advisor speaks MiniMax, a DeepSeek advisor speaks DeepSeek).
    pub fn advisor_model(&self) -> Option<Arc<dyn Model>> {
        let a = self.advisor.as_ref()?;
        let mut m = OpenAICompatModel::new(a.base_url.clone(), a.model.clone())
            .with_thinking(self.thinking_enabled, self.reasoning_effort.clone());
        if let Some(k) = &a.api_key {
            m = m.with_api_key(k.clone());
        }
        Some(Arc::new(m))
    }

    fn build(&self, name: &str) -> OpenAICompatModel {
        let mut m = OpenAICompatModel::new(self.base_url.clone(), name.to_string())
            .with_thinking(self.thinking_enabled, self.reasoning_effort.clone());
        if let Some(k) = &self.api_key {
            m = m.with_api_key(k.clone());
        }
        m
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

    #[test]
    fn missing_base_url_errors() {
        scoped(
            &[
                ("MODEL_BASE_URL", None),
                ("MODEL_NAME_FAST", Some("x")),
                ("MODEL_NAME_DEEP", Some("y")),
            ],
            || {
                let err = ModelConfig::from_current_env().unwrap_err();
                assert!(format!("{err}").contains("MODEL_BASE_URL"));
            },
        );
    }

    #[test]
    fn missing_both_fast_and_default_errors() {
        scoped(
            &[
                ("MODEL_BASE_URL", Some("https://x/v1")),
                ("MODEL_NAME_FAST", None),
                ("MODEL_NAME_DEEP", Some("d")),
                ("MODEL_NAME_DEFAULT", None),
            ],
            || {
                let err = ModelConfig::from_current_env().unwrap_err();
                assert!(format!("{err}").contains("MODEL_NAME_FAST"));
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
            .with_advisor("https://api.deepseek.com/v1", Some("sk-adv".into()), "deepseek-v4-pro");
        let advisor = cfg.advisor_model().expect("advisor should be Some");
        assert_eq!(advisor.name(), "deepseek-v4-pro");
    }
}
