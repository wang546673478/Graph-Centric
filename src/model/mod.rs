//! Model trait surface and concrete implementations.
//!
//! Per design principle #1 (model-agnostic), the harness only depends on
//! the `Model` trait. Concrete backends plug in here.

pub mod capabilities;
pub mod config;
pub mod openai_compat;
pub mod streaming;

pub use capabilities::{ModelCapabilities, ReasoningField, ThinkingStyle};
pub use config::ModelConfig;
pub use openai_compat::OpenAICompatModel;
pub use streaming::ModelWithEvents;

use crate::error::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<serde_json::Value>,
    pub temperature: f64,
    pub max_tokens: Option<usize>,
    pub stop: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    pub usage: Usage,
}

impl ModelResponse {
    pub fn new(content: impl Into<String>, finish_reason: FinishReason, usage: Usage) -> Self {
        Self { content: content.into(), reasoning_content: None, tool_calls: vec![], finish_reason, usage }
    }

    /// Best-effort parser text for callers that need the model's response
    /// as a string (decomposer, repairer, verifier, reviewer, cascade, etc.).
    ///
    /// Returns `content` when it's non-blank, otherwise falls back to
    /// `reasoning_content`. The fallback handles reasoning-style models
    /// (DeepSeek, MiniMax M3) that emit their final JSON in
    /// `reasoning_content` while leaving `content` empty — without it,
    /// parsers die with `proposer: no '{' in response` on the db2d993d
    /// failure mode. Both fields blank → returns `""`; callers that
    /// require *some* text should check `.trim().is_empty()` themselves
    /// and surface a clear error (e.g. `decomposer: empty response`).
    pub fn text_or_reasoning(&self) -> &str {
        if self.content.trim().is_empty() {
            self.reasoning_content.as_deref().unwrap_or("")
        } else {
            self.content.as_str()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    MaxTokens,
    Error,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
    #[serde(default)]
    pub prompt_cache_hit_tokens: usize,
    #[serde(default)]
    pub prompt_cache_miss_tokens: usize,
}

/// A chunk of streaming output — either a content delta or a terminal done signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum StreamDelta {
    Delta {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },
    Done { finish_reason: FinishReason, usage: Usage },
}

#[async_trait]
pub trait Model: Send + Sync {
    fn name(&self) -> &str;
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse>;

    /// Streaming variant. Default implementation falls back to `complete()`
    /// and emits one `Delta` followed by a `Done`. Override for true SSE streaming.
    async fn complete_stream(
        &self,
        request: ModelRequest,
        tx: tokio::sync::mpsc::UnboundedSender<StreamDelta>,
    ) -> Result<ModelResponse> {
        let resp = self.complete(request).await?;
        let _ = tx.send(StreamDelta::Delta {
            content: resp.content.clone(),
            reasoning_content: resp.reasoning_content.clone(),
        });
        let _ = tx.send(StreamDelta::Done {
            finish_reason: resp.finish_reason,
            usage: resp.usage.clone(),
        });
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp_with(content: &str, reasoning: Option<&str>) -> ModelResponse {
        ModelResponse {
            content: content.to_string(),
            reasoning_content: reasoning.map(str::to_string),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        }
    }

    /// Content wins when it's non-blank (the conventional channel).
    #[test]
    fn text_or_reasoning_prefers_content() {
        let r = resp_with("from content", Some("from reasoning"));
        assert_eq!(r.text_or_reasoning(), "from content");
    }

    /// Reasoning-content fallback fires when content is empty —
    /// the DeepSeek / M3 failure mode that killed db2d993d.
    #[test]
    fn text_or_reasoning_falls_back_when_content_empty() {
        let r = resp_with("", Some("from reasoning"));
        assert_eq!(r.text_or_reasoning(), "from reasoning");
    }

    /// Whitespace-only content also triggers the fallback (trim() check).
    #[test]
    fn text_or_reasoning_falls_back_on_whitespace_content() {
        let r = resp_with("   \n\t  ", Some("from reasoning"));
        assert_eq!(r.text_or_reasoning(), "from reasoning");
    }

    /// Both empty → empty string; caller is responsible for the
    /// clear "empty response" error rather than a confusing
    /// `proposer: no '{' in response`.
    #[test]
    fn text_or_reasoning_empty_when_both_blank() {
        let r = resp_with("", None);
        assert_eq!(r.text_or_reasoning(), "");
    }
}
