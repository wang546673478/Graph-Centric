//! Model trait surface and concrete implementations.
//!
//! Per design principle #1 (model-agnostic), the harness only depends on
//! the `Model` trait. Concrete backends plug in here.

pub mod config;
pub mod openai_compat;
pub mod streaming;

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
