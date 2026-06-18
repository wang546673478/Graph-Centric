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

#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub fast: String,
    pub deep: String,
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
        })
    }

    /// Build from web engine config (JSON file) — bypasses env vars entirely.
    pub fn from_engine_config(cfg: &crate::web::state::ModelTierConfig) -> Self {
        Self {
            base_url: cfg.base_url.clone(),
            api_key: if cfg.api_key.is_empty() { None } else { Some(cfg.api_key.clone()) },
            fast: cfg.fast_model.clone(),
            deep: cfg.deep_model.clone(),
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
        }
    }

    /// Build the **fast-tier** model client.
    pub fn fast_model(&self) -> Arc<dyn Model> {
        Arc::new(self.build(&self.fast))
    }

    /// Build the **deep-tier** model client.
    pub fn deep_model(&self) -> Arc<dyn Model> {
        Arc::new(self.build(&self.deep))
    }

    fn build(&self, name: &str) -> OpenAICompatModel {
        let mut m = OpenAICompatModel::new(self.base_url.clone(), name.to_string());
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
}
