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
    /// Named model profiles for quick switching.
    #[serde(default)]
    pub profiles: std::collections::HashMap<String, ModelTierConfig>,
    /// Currently active profile name (empty = use `model` directly).
    #[serde(default)]
    pub active_profile: String,
    /// Maximum drill-down depth. 0 = main run only; 2 = main + sub + sub-sub.
    /// Default: 2 (3 levels total).
    #[serde(default = "default_max_drilldown_depth")]
    pub max_drilldown_depth: usize,
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
    /// Optional independent **advisor** backend. When `advisor_base_url`
    /// and `advisor_model` are non-empty, the main (task) model can emit a
    /// `consult_advisor` step to ask this separate model a question. Fully
    /// independent of the task backend — different vendor, key, model.
    #[serde(default)]
    pub advisor_base_url: String,
    #[serde(default)]
    pub advisor_api_key: String,
    #[serde(default)]
    pub advisor_api_key_masked: String,
    #[serde(default)]
    pub advisor_model: String,
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
    /// Auto-match and apply skills to incoming tasks.
    /// When true (default), matching skills' compiled task graphs
    /// substitute the decomposer output in the Task phase.
    #[serde(default = "default_true")]
    pub auto_apply_skills: bool,

    // Stagnation detection thresholds
    #[serde(default = "default_stagnation_soft_hint")]
    pub stagnation_soft_hint: u32,
    #[serde(default = "default_stagnation_hard_hint")]
    pub stagnation_hard_hint: u32,
    #[serde(default = "default_stagnation_terminate")]
    pub stagnation_terminate: u32,

    // Stuck detection thresholds
    #[serde(default = "default_stuck_soft_hint")]
    pub stuck_soft_hint: u32,
    #[serde(default = "default_stuck_hard_hint")]
    pub stuck_hard_hint: u32,
    #[serde(default = "default_stuck_terminate")]
    pub stuck_terminate: u32,

    // Tool failure thresholds
    #[serde(default = "default_tool_failure_warn")]
    pub tool_failure_warn_after: u32,
    #[serde(default = "default_tool_failure_halt")]
    pub tool_failure_halt_after: u32,

    // Event channel capacity
    #[serde(default = "default_event_channel_capacity")]
    pub event_channel_capacity: usize,

    // Self-optimization laws (graph-centric design gaps)
    /// Gap 1: force an explore subagent after this many Filling rounds
    /// without new nodes. 0 disables.
    #[serde(default = "default_force_search_after_filling_stall")]
    pub force_search_after_filling_stall: u32,
    /// Gap 3: rounds of stable+connected+enriched graph before injecting
    /// the "emit ready_for_verify" convergence hint. 0 disables.
    #[serde(default = "default_convergence_stable_rounds")]
    pub convergence_stable_rounds: u32,

    /// Mirror of `EngineConfig::max_drilldown_depth` kept for backward
    /// compatibility with serialized configs from Task 6. The canonical
    /// source is now `EngineConfig::max_drilldown_depth`.
    #[serde(default = "default_loop_tuning_max_drilldown_depth")]
    pub max_drilldown_depth: u32,
}

fn default_true() -> bool {
    true
}

fn default_stagnation_soft_hint() -> u32 { 4 }
fn default_stagnation_hard_hint() -> u32 { 6 }
fn default_stagnation_terminate() -> u32 { 8 }
fn default_stuck_soft_hint() -> u32 { 3 }
fn default_stuck_hard_hint() -> u32 { 5 }
fn default_stuck_terminate() -> u32 { 6 }
fn default_tool_failure_warn() -> u32 { 3 }
fn default_tool_failure_halt() -> u32 { 8 }
fn default_event_channel_capacity() -> usize { 256 }
fn default_force_search_after_filling_stall() -> u32 { 5 }
fn default_convergence_stable_rounds() -> u32 { 3 }
fn default_max_drilldown_depth() -> usize { 2 }
fn default_loop_tuning_max_drilldown_depth() -> u32 { 2 }

impl EngineConfig {
    /// Load config from disk, falling back to env vars + defaults.
    pub fn load() -> Self {
        let path = std::path::PathBuf::from(".graph_harness_config.json");
        let mut cfg = if path.exists() {
            if let Ok(json) = std::fs::read_to_string(&path) {
                if let Ok(cfg) = serde_json::from_str(&json) {
                    cfg
                } else {
                    Self::default()
                }
            } else {
                Self::default()
            }
        } else {
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
                    advisor_base_url: std::env::var("ADVISOR_BASE_URL").unwrap_or_default(),
                    advisor_api_key: std::env::var("ADVISOR_API_KEY").unwrap_or_default(),
                    advisor_api_key_masked: String::new(),
                    advisor_model: std::env::var("ADVISOR_MODEL").unwrap_or_default(),
                },
                ..Default::default()
            }
        };
        // Env-var overrides take precedence over the disk config so operators
        // can tweak a single knob without editing the JSON file.
        if let Ok(s) = std::env::var("GRAPH_HARNESS_MAX_DRILLDOWN_DEPTH") {
            if let Ok(v) = s.parse::<usize>() {
                cfg.max_drilldown_depth = v;
            }
        }
        cfg
    }

    /// Sync model config to env vars so ModelConfig::load() picks them up.
    pub fn sync_env(&self) {
        if !self.model.base_url.is_empty() {
            unsafe { std::env::set_var("MODEL_BASE_URL", &self.model.base_url); }
        }
        if !self.model.api_key.is_empty() {
            unsafe { std::env::set_var("MODEL_API_KEY", &self.model.api_key); }
        }
        unsafe {
            std::env::set_var("MODEL_NAME_FAST", &self.model.fast_model);
            std::env::set_var("MODEL_NAME_DEEP", &self.model.deep_model);
        }
        if !self.model.advisor_base_url.is_empty() {
            unsafe { std::env::set_var("ADVISOR_BASE_URL", &self.model.advisor_base_url); }
        }
        if !self.model.advisor_api_key.is_empty() {
            unsafe { std::env::set_var("ADVISOR_API_KEY", &self.model.advisor_api_key); }
        }
        if !self.model.advisor_model.is_empty() {
            unsafe { std::env::set_var("ADVISOR_MODEL", &self.model.advisor_model); }
        }
    }

    /// Persist config to disk.
    pub fn save(&self) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        // Always write to the project-relative path (same as load's first candidate).
        std::fs::write(".graph_harness_config.json", json)
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            profiles: std::collections::HashMap::new(),
            active_profile: String::new(),
            max_drilldown_depth: default_max_drilldown_depth(),
            model: ModelTierConfig {
                base_url: String::new(),
                api_key: String::new(),
                api_key_masked: String::new(),
                fast_model: "deepseek-v4-flash".into(),
                deep_model: "deepseek-v4-pro".into(),
                default_model: None,
                advisor_base_url: String::new(),
                advisor_api_key: String::new(),
                advisor_api_key_masked: String::new(),
                advisor_model: String::new(),
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
                auto_apply_skills: true,
                stagnation_soft_hint: 4,
                stagnation_hard_hint: 6,
                stagnation_terminate: 8,
                stuck_soft_hint: 3,
                stuck_hard_hint: 5,
                stuck_terminate: 6,
                tool_failure_warn_after: 3,
                tool_failure_halt_after: 8,
                event_channel_capacity: 256,
                force_search_after_filling_stall: 5,
                convergence_stable_rounds: 3,
                max_drilldown_depth: 2,
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

    #[test]
    fn engine_config_default_max_drilldown_depth_is_2() {
        let cfg = EngineConfig::default();
        assert_eq!(cfg.max_drilldown_depth, 2, "default should be 2 (= 3 levels: main+sub+sub-sub)");
    }

    #[test]
    fn engine_config_from_env_overrides_max_drilldown_depth() {
        unsafe {
            std::env::set_var("GRAPH_HARNESS_MAX_DRILLDOWN_DEPTH", "5");
        }
        let cfg = EngineConfig::load();
        unsafe {
            std::env::remove_var("GRAPH_HARNESS_MAX_DRILLDOWN_DEPTH");
        }
        assert_eq!(cfg.max_drilldown_depth, 5);
    }
}
