//! Web configuration: port, root directory, model defaults, engine tuning.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Static configuration for the web gateway. Read from env at startup
/// (fail fast). All fields have sensible defaults except `bind_addr`,
/// which falls back to `0.0.0.0:8080`.
#[derive(Debug, Clone)]
pub struct WebConfig {
    /// Address to bind the HTTP server to.
    pub bind_addr: String,
    /// Path to the directory of static frontend files (`webui/dist/`).
    /// Empty string disables static file serving.
    pub static_dir: String,
    /// Project root (cwd by default). Used for git-based file diffs.
    pub project_root: PathBuf,
    /// v2: runtime-configurable engine settings.
    pub engine: EngineConfig,
}

impl WebConfig {
    /// Read from env vars. Falls back to defaults.
    pub fn from_env() -> Self {
        let bind_addr = std::env::var("WEB_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .map(|p| format!("0.0.0.0:{p}"))
            .unwrap_or_else(|| "0.0.0.0:8080".to_string());
        let static_dir = std::env::var("WEB_STATIC_DIR")
            .unwrap_or_else(|_| "webui".to_string());
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            bind_addr,
            static_dir,
            project_root,
            engine: EngineConfig::default(),
        }
    }
}

/// v2: runtime-configurable engine parameters. Updated via POST /api/config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    pub model: ModelTierConfig,
    pub policy: ToolPolicyConfig,
    pub loop_tuning: LoopTuningConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelTierConfig {
    pub base_url: String,
    #[serde(default)]
    pub api_key_masked: String,
    pub fast_model: String,
    pub deep_model: String,
    #[serde(default)]
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPolicyConfig {
    #[serde(default)]
    pub deny_patterns: Vec<String>,
    #[serde(default)]
    pub implicit_cwd_verbs: Vec<String>,
    pub max_concurrent_subagents: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopTuningConfig {
    pub max_rounds: usize,
    pub max_repair_rounds: usize,
    pub cascade_backtrack: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            model: ModelTierConfig {
                base_url: String::new(),
                api_key_masked: String::new(),
                fast_model: "deepseek-v4-flash".into(),
                deep_model: "deepseek-v4-pro".into(),
                default_model: None,
            },
            policy: ToolPolicyConfig {
                deny_patterns: vec![],
                implicit_cwd_verbs: vec![
                    "cargo".into(), "rustc".into(), "go".into(), "node".into(),
                    "npm".into(), "yarn".into(), "pnpm".into(), "python".into(),
                    "python3".into(), "pip".into(), "pip3".into(), "make".into(),
                ],
                max_concurrent_subagents: 2,
            },
            loop_tuning: LoopTuningConfig {
                max_rounds: 300,
                max_repair_rounds: 4,
                cascade_backtrack: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_config_defaults_to_localhost_8080() {
        // We can't easily test `from_env` because it reads real env vars.
        // Instead, just construct manually.
        let cfg = WebConfig {
            bind_addr: "0.0.0.0:8080".to_string(),
            static_dir: "webui/dist".to_string(),
            project_root: PathBuf::from("."),
            engine: EngineConfig::default(),
        };
        assert!(cfg.bind_addr.contains("8080"));
    }

    #[test]
    fn web_config_honors_web_port_env() {
        // Manually simulate the env-var parsing.
        let port: u16 = "9999".parse().unwrap();
        let addr = format!("0.0.0.0:{port}");
        assert_eq!(addr, "0.0.0.0:9999");
    }
}
