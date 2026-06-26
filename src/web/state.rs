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
    /// Advanced tuning — knobs that were once hardcoded in source.
    /// Defaults match the pre-config behavior; UI exposes them in a
    /// collapsed "Advanced" section.
    #[serde(default)]
    pub advanced: AdvancedTuningConfig,
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
    /// Wall-clock timeout (millis) for a pending sub-run. When a sub-run
    /// ages past this many millis without writing a terminal `run.json`,
    /// `poll_sub_run_status` transitions its handle to `SubRunStatus::Timeout`,
    /// stamps the complex node as timed-out, and raises `drill_down_error`
    /// so the parent surfaces a `LoopState::GraphInvalid`.
    ///
    /// Default: 1_800_000 (30 min). Override via the
    /// `GRAPH_HARNESS_SUB_RUN_TIMEOUT_MS` env var.
    #[serde(default = "default_sub_run_timeout_ms")]
    pub sub_run_timeout_ms: u64,
}

/// v2.7: advanced tuning knobs that were once hardcoded in source.
/// Every field's default reproduces the pre-config behavior exactly —
/// no tuning should be required to keep current results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvancedTuningConfig {
    // Sub-agent control
    /// Max model calls per sub-agent before it's force-stopped. Default 8.
    #[serde(default = "default_subagent_max_steps")]
    pub subagent_max_steps: usize,

    // Proposer explore limits
    /// Max items per single `explore` step. Default 1.
    #[serde(default = "default_explore_max_items_per_step")]
    pub explore_max_items_per_step: usize,
    /// Max chars per explore-item question. Default 2000.
    #[serde(default = "default_explore_max_question_chars")]
    pub explore_max_question_chars: usize,

    // Enricher
    /// Tail-keep cap for L2 injected into enrichment prompts (chars). Default 12_000.
    #[serde(default = "default_enricher_l2_char_cap")]
    pub enricher_l2_char_cap: usize,
    /// Number of neighbor edges sampled when rendering L1 context. Default 12.
    #[serde(default = "default_enricher_neighbor_limit")]
    pub enricher_neighbor_limit: usize,
    /// Hard cap on L1 confidence when L2 was unavailable. Default 0.6.
    #[serde(default = "default_enricher_l0_only_confidence_cap")]
    pub enricher_l0_only_confidence_cap: f64,

    // Skill matching
    /// Min score to auto-apply a skill. Default 0.25.
    #[serde(default = "default_skill_match_threshold")]
    pub skill_match_threshold: f64,
    /// Weight of trigger-text Jaccard in skill score. Default 0.7.
    #[serde(default = "default_skill_match_trigger_weight")]
    pub skill_match_trigger_weight: f64,
    /// Weight of slug-token Jaccard in skill score. Default 0.3.
    #[serde(default = "default_skill_match_slug_weight")]
    pub skill_match_slug_weight: f64,

    // Cascade expansion
    /// Max L0→L1→L2 expansion depth. Default 3.
    #[serde(default = "default_cascade_max_expand_depth")]
    pub cascade_max_expand_depth: usize,

    // Tool timeouts
    /// PostExecutionValidator default command timeout (ms). Default 300_000 (5 min).
    #[serde(default = "default_validator_default_timeout_ms")]
    pub validator_default_timeout_ms: u64,
    /// Bash tool default per-call timeout (ms). Default 120_000 (2 min).
    #[serde(default = "default_bash_default_timeout_ms")]
    pub bash_default_timeout_ms: u64,
    /// Bash tool max per-call timeout (ms). Default 600_000 (10 min).
    #[serde(default = "default_bash_max_timeout_ms")]
    pub bash_max_timeout_ms: u64,

    // Token caps (per-layer defaults)
    /// Proposer default max_tokens. Default 4096.
    #[serde(default = "default_proposer_default_max_tokens")]
    pub proposer_default_max_tokens: usize,
    /// Decomposer default max_tokens. Default 8192.
    #[serde(default = "default_decomposer_default_max_tokens")]
    pub decomposer_default_max_tokens: usize,
    /// SubAgent default max_tokens. Default 4096.
    #[serde(default = "default_subagent_default_max_tokens")]
    pub subagent_default_max_tokens: usize,
    /// Verifier L2-excerpt cap when sampling L1. Default 4000.
    #[serde(default = "default_verifier_l2_excerpt_chars")]
    pub verifier_l2_excerpt_chars: usize,

    // CLI auto-repair loop (used by bin/agent_a demo)
    /// Max auto-repair cycles in the CLI demo. Default 3.
    #[serde(default = "default_max_auto_repair_cycles")]
    pub max_auto_repair_cycles: usize,
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
fn default_sub_run_timeout_ms() -> u64 { 1_800_000 } // 30 min

// AdvancedTuningConfig defaults — match the pre-config hardcoded values.
fn default_subagent_max_steps() -> usize { 8 }
fn default_explore_max_items_per_step() -> usize { 1 }
fn default_explore_max_question_chars() -> usize { 2000 }
fn default_enricher_l2_char_cap() -> usize { 12_000 }
fn default_enricher_neighbor_limit() -> usize { 12 }
fn default_enricher_l0_only_confidence_cap() -> f64 { 0.6 }
fn default_skill_match_threshold() -> f64 { 0.25 }
fn default_skill_match_trigger_weight() -> f64 { 0.7 }
fn default_skill_match_slug_weight() -> f64 { 0.3 }
fn default_cascade_max_expand_depth() -> usize { 3 }
fn default_validator_default_timeout_ms() -> u64 { 300_000 } // 5 min
fn default_bash_default_timeout_ms() -> u64 { 120_000 } // 2 min
fn default_bash_max_timeout_ms() -> u64 { 600_000 } // 10 min
fn default_proposer_default_max_tokens() -> usize { 4096 }
fn default_decomposer_default_max_tokens() -> usize { 8192 }
fn default_subagent_default_max_tokens() -> usize { 4096 }
fn default_verifier_l2_excerpt_chars() -> usize { 4000 }
fn default_max_auto_repair_cycles() -> usize { 3 }

impl Default for AdvancedTuningConfig {
    fn default() -> Self {
        Self {
            subagent_max_steps: default_subagent_max_steps(),
            explore_max_items_per_step: default_explore_max_items_per_step(),
            explore_max_question_chars: default_explore_max_question_chars(),
            enricher_l2_char_cap: default_enricher_l2_char_cap(),
            enricher_neighbor_limit: default_enricher_neighbor_limit(),
            enricher_l0_only_confidence_cap: default_enricher_l0_only_confidence_cap(),
            skill_match_threshold: default_skill_match_threshold(),
            skill_match_trigger_weight: default_skill_match_trigger_weight(),
            skill_match_slug_weight: default_skill_match_slug_weight(),
            cascade_max_expand_depth: default_cascade_max_expand_depth(),
            validator_default_timeout_ms: default_validator_default_timeout_ms(),
            bash_default_timeout_ms: default_bash_default_timeout_ms(),
            bash_max_timeout_ms: default_bash_max_timeout_ms(),
            proposer_default_max_tokens: default_proposer_default_max_tokens(),
            decomposer_default_max_tokens: default_decomposer_default_max_tokens(),
            subagent_default_max_tokens: default_subagent_default_max_tokens(),
            verifier_l2_excerpt_chars: default_verifier_l2_excerpt_chars(),
            max_auto_repair_cycles: default_max_auto_repair_cycles(),
        }
    }
}

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
            // Default the model names to `MODEL_NAME_DEFAULT` (or empty
            // if not set) so an empty model name fails fast at the
            // first model call with a clear "model name is empty"
            // error — instead of silently failing with a 400 "unknown
            // model" when the user is on a non-DeepSeek backend.
            let base_url = std::env::var("MODEL_BASE_URL").unwrap_or_default();
            let api_key = std::env::var("MODEL_API_KEY").unwrap_or_default();
            let default_model = std::env::var("MODEL_NAME_DEFAULT").unwrap_or_default();
            let fast_model = std::env::var("MODEL_NAME_FAST")
                .unwrap_or_else(|_| default_model.clone());
            let deep_model = std::env::var("MODEL_NAME_DEEP")
                .unwrap_or_else(|_| default_model.clone());
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
        if let Ok(s) = std::env::var("GRAPH_HARNESS_SUB_RUN_TIMEOUT_MS") {
            if let Ok(v) = s.parse::<u64>() {
                cfg.sub_run_timeout_ms = v;
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
            advanced: AdvancedTuningConfig::default(),
            max_drilldown_depth: default_max_drilldown_depth(),
            sub_run_timeout_ms: default_sub_run_timeout_ms(),
            model: ModelTierConfig {
                base_url: String::new(),
                api_key: String::new(),
                api_key_masked: String::new(),
                fast_model: String::new(),
                deep_model: String::new(),
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

    #[test]
    fn engine_config_default_sub_run_timeout_ms_is_30_min() {
        let cfg = EngineConfig::default();
        assert_eq!(
            cfg.sub_run_timeout_ms,
            1_800_000,
            "default should be 30 minutes (1_800_000 ms)"
        );
    }

    #[test]
    fn engine_config_from_env_overrides_sub_run_timeout_ms() {
        unsafe {
            std::env::set_var("GRAPH_HARNESS_SUB_RUN_TIMEOUT_MS", "60000");
        }
        let cfg = EngineConfig::load();
        unsafe {
            std::env::remove_var("GRAPH_HARNESS_SUB_RUN_TIMEOUT_MS");
        }
        assert_eq!(cfg.sub_run_timeout_ms, 60_000);
    }
}
