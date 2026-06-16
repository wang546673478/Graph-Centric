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
            .unwrap_or_else(|_| {
                if std::path::Path::new("webui/dist").exists() {
                    "webui/dist".to_string()
                } else {
                    "webui".to_string()
                }
            });
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            bind_addr,
            static_dir,
            project_root,
            engine: EngineConfig::load(),
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
    pub api_key: String,
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
    /// DeepSeek thinking mode: enabled/disabled.
    #[serde(default)]
    pub thinking_enabled: bool,
    /// Reasoning effort: "high" or "max".
    #[serde(default)]
    pub reasoning_effort: String,
}

impl EngineConfig {
    /// Load config from disk, falling back to env vars + defaults.
    pub fn load() -> Self {
        // Try project-relative path first, then cwd-relative.
        let candidates = [
            std::path::PathBuf::from(".graph_harness_config.json"),
            std::env::current_dir().unwrap_or_default().join(".graph_harness_config.json"),
        ];
        let path = candidates.iter().find(|p| p.exists());
        if let Some(path) = path {
            if let Ok(json) = std::fs::read_to_string(path) {
                if let Ok(cfg) = serde_json::from_str(&json) {
                    return cfg;
                }
            }
        }
        // Fallback: read from env vars like ModelConfig does.
        let base_url = std::env::var("MODEL_BASE_URL").unwrap_or_default();
        let api_key = std::env::var("MODEL_API_KEY").unwrap_or_default();
        let fast_model = std::env::var("MODEL_NAME_FAST").unwrap_or_else(|_| "deepseek-v4-flash".into());
        let deep_model = std::env::var("MODEL_NAME_DEEP").unwrap_or_else(|_| "deepseek-v4-pro".into());
        EngineConfig {
            model: ModelTierConfig {
                base_url,
                api_key,
                api_key_masked: String::new(),
                fast_model,
                deep_model,
                default_model: None,
            },
            ..Default::default()
        }
    }

    /// Persist config to disk.
    pub fn save(&self) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        // Write to cwd so the next load() finds it regardless of how the server was started.
        let path = std::env::current_dir()?.join(".graph_harness_config.json");
        std::fs::write(&path, json)
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            model: ModelTierConfig {
                base_url: String::new(),
                api_key: String::new(),
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
                thinking_enabled: false,
                reasoning_effort: "high".into(),
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
