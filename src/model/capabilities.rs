//! Model capability descriptors.
//!
//! Per design principle #1 (model-agnostic), the agent core only depends on
//! the `Model` trait and the unified `ModelResponse`. But different backends
//! expose the *same* OpenAI-compatible wire protocol with different private
//! extensions: DeepSeek streams chain-of-thought in a `reasoning_content`
//! field, MiniMax M3 wraps it in `<think>` tags inside `content` (unless you
//! flip `reasoning_split`), thinking toggles differ, temperature ranges
//! differ, and so on.
//!
//! Rather than branch on model *name* throughout the codebase (the thing
//! Claude Code avoids — it adapts by capability, not by identity), we
//! capture those differences once in [`ModelCapabilities`] and let
//! [`ModelCapabilities::from_model_name`] infer them from the configured
//! model string. The HTTP client (`AnthropicModel`) reads the capability
//! struct when building requests and parsing responses; the agent core
//! never sees any of it.
//!
//! Adding a new backend = add one match arm here, not a new code path.

/// How a backend returns its chain-of-thought / reasoning text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningField {
    /// No separate reasoning channel; everything is in `content`.
    None,
    /// A dedicated `reasoning_content` field on the response message
    /// (DeepSeek, MiniMax with `reasoning_split=true`).
    ReasoningContent,
    /// Reasoning is wrapped in `<think>...</think>` tags inside `content`
    /// (MiniMax native mode). The JSON extractor strips these, but we
    /// prefer to request the split form so `content` stays clean.
    ThinkTag,
}

/// How a backend wants the "enable/disable thinking" request flag shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingStyle {
    /// Backend has no thinking toggle; omit any thinking field.
    None,
    /// DeepSeek-style: a top-level `reasoning_effort` string ("high"/"max").
    DeepSeek,
    /// MiniMax M3-style: a `thinking` object `{"type":"adaptive"|"disabled"}`
    /// plus `reasoning_split:true` so reasoning returns in its own field.
    MiniMax,
}

/// Per-model behavioral knobs the HTTP client consults when building a
/// request body and parsing a response. Inferred from the model name, but
/// can be overridden programmatically.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelCapabilities {
    /// Backend supports native OpenAI `tool_calls`. When false, the
    /// Proposer falls back to text-JSON parsing only.
    pub native_tools: bool,
    /// Where the model puts its reasoning text.
    pub reasoning_field: ReasoningField,
    /// Whether thinking can be toggled on/off via a request field.
    pub supports_thinking_toggle: bool,
    /// Shape of the thinking request field, if any.
    pub thinking_style: ThinkingStyle,
    /// Maximum accepted `temperature`. Requests above this are clamped.
    pub temperature_max: f64,
    /// Prefer the newer `max_completion_tokens` field over the legacy
    /// `max_tokens` (MiniMax recommends the former).
    pub prefers_max_completion_tokens: bool,
}

impl Default for ModelCapabilities {
    /// Conservative defaults for an unknown OpenAI-compatible backend:
    /// native tools on (almost universal now), no reasoning channel, no
    /// thinking toggle, the OpenAI temperature range [0, 2], legacy
    /// `max_tokens`.
    fn default() -> Self {
        Self {
            native_tools: true,
            reasoning_field: ReasoningField::None,
            supports_thinking_toggle: false,
            thinking_style: ThinkingStyle::None,
            temperature_max: 2.0,
            prefers_max_completion_tokens: false,
        }
    }
}

impl ModelCapabilities {
    /// Infer capabilities from the configured model name. Matching is
    /// case-insensitive and substring-based so variants like
    /// `deepseek-v4-pro`, `MiniMax-M3`, `minimax-m2.7-highspeed` all map.
    pub fn from_model_name(name: &str) -> Self {
        let n = name.to_ascii_lowercase();
        if n.contains("minimax") {
            // MiniMax M3 (and the M2.x family): OpenAI-compatible, native
            // tools, reasoning via `<think>` natively but we request the
            // split form, thinking object toggle, temperature up to 2.0,
            // prefers max_completion_tokens.
            Self {
                native_tools: true,
                reasoning_field: ReasoningField::ReasoningContent,
                supports_thinking_toggle: true,
                thinking_style: ThinkingStyle::MiniMax,
                temperature_max: 2.0,
                prefers_max_completion_tokens: true,
            }
        } else if n.contains("deepseek") {
            // DeepSeek: native tools, dedicated reasoning_content field,
            // reasoning_effort toggle, temperature up to 2.0.
            Self {
                native_tools: true,
                reasoning_field: ReasoningField::ReasoningContent,
                supports_thinking_toggle: true,
                thinking_style: ThinkingStyle::DeepSeek,
                temperature_max: 2.0,
                prefers_max_completion_tokens: false,
            }
        } else {
            Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimax_m3_is_detected() {
        let c = ModelCapabilities::from_model_name("MiniMax-M3");
        assert_eq!(c.thinking_style, ThinkingStyle::MiniMax);
        assert!(c.prefers_max_completion_tokens);
        assert_eq!(c.reasoning_field, ReasoningField::ReasoningContent);
    }

    #[test]
    fn minimax_variants_match_case_insensitively() {
        for name in ["minimax-m2.7-highspeed", "MINIMAX-M3", "MiniMax-M2"] {
            assert_eq!(
                ModelCapabilities::from_model_name(name).thinking_style,
                ThinkingStyle::MiniMax,
                "failed for {name}"
            );
        }
    }

    #[test]
    fn deepseek_uses_reasoning_effort_style() {
        let c = ModelCapabilities::from_model_name("deepseek-v4-pro");
        assert_eq!(c.thinking_style, ThinkingStyle::DeepSeek);
        assert!(!c.prefers_max_completion_tokens);
    }

    #[test]
    fn unknown_model_falls_back_to_conservative_defaults() {
        let c = ModelCapabilities::from_model_name("some-local-llama");
        assert_eq!(c, ModelCapabilities::default());
        assert_eq!(c.thinking_style, ThinkingStyle::None);
        assert!(c.native_tools);
    }
}
