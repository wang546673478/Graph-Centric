//! OpenAI-compatible HTTP client.
//!
//! Speaks the `/v1/chat/completions` wire protocol, which is the lingua
//! franca of nearly every modern inference backend: Ollama, vLLM, LM Studio,
//! OpenRouter, real OpenAI, Anthropic-via-proxy, LiteLLM, etc. Picking this
//! one client gets us model-agnostic access (principle #1).
//!
//! Configuration is environment-first:
//!
//! - `MODEL_BASE_URL`  default `http://localhost:11434/v1` (Ollama)
//! - `MODEL_NAME`      required — no default; the user must declare which
//!                     model is being used so logs/telemetry are honest
//! - `MODEL_API_KEY`   optional; sent as `Authorization: Bearer …`

use super::{FinishReason, Message, Model, ModelRequest, ModelResponse, Role, ToolCall, Usage};
use crate::error::{HarnessError, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct OpenAICompatModel {
    pub base_url: String,
    pub model_name: String,
    pub api_key: Option<String>,
    client: Client,
}

impl OpenAICompatModel {
    pub fn new(base_url: impl Into<String>, model_name: impl Into<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("reqwest client builds with default settings");
        Self {
            base_url: base_url.into(),
            model_name: model_name.into(),
            api_key: None,
            client,
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Build from environment.
    ///
    /// - `MODEL_NAME` is required; no default. We refuse to silently pick a
    ///   model because that hides what the run actually did.
    /// - `MODEL_BASE_URL` defaults to Ollama on localhost.
    /// - `MODEL_API_KEY` is optional.
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("MODEL_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434/v1".to_string());
        let model_name = std::env::var("MODEL_NAME").map_err(|_| {
            HarnessError::model("MODEL_NAME env var is required (no default)")
        })?;
        let api_key = std::env::var("MODEL_API_KEY").ok();
        let mut m = Self::new(base_url, model_name);
        if let Some(k) = api_key {
            m = m.with_api_key(k);
        }
        Ok(m)
    }
}

// ---------------------------------------------------------------------------
// Wire types — kept private; the public API is the `Model` trait
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct OpenAIChatRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct OpenAIMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIChatResponse {
    choices: Vec<OpenAIChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    message: OpenAIResponseMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OpenAIToolCall {
    id: String,
    #[serde(rename = "type", default)]
    _kind: String,
    function: OpenAIFunctionCall,
}

#[derive(Debug, Deserialize)]
struct OpenAIFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIUsage {
    #[serde(default)]
    prompt_tokens: usize,
    #[serde(default)]
    completion_tokens: usize,
    #[serde(default)]
    total_tokens: usize,
}

// ---------------------------------------------------------------------------
// Model trait impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Model for OpenAICompatModel {
    fn name(&self) -> &str {
        &self.model_name
    }

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let messages: Vec<OpenAIMessage> = request
            .messages
            .into_iter()
            .map(|m| OpenAIMessage {
                role: role_to_str(m.role).to_string(),
                content: m.content,
            })
            .collect();

        let body = OpenAIChatRequest {
            model: self.model_name.clone(),
            messages,
            temperature: Some(request.temperature),
            max_tokens: request.max_tokens,
            stop: request.stop,
            tools: request.tools,
        };

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut req = self.client.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| HarnessError::model(format!("HTTP request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(HarnessError::model(format!(
                "HTTP {status} from {url}: {body}"
            )));
        }

        let parsed: OpenAIChatResponse = resp
            .json()
            .await
            .map_err(|e| HarnessError::model(format!("JSON parse failed: {e}")))?;

        let first = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| HarnessError::model("no choices in model response"))?;

        let content = first.message.content.unwrap_or_default();
        let tool_calls: Vec<ToolCall> = first
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| {
                // arguments comes as a JSON string in OpenAI; try to parse it,
                // fall back to a string value if it isn't valid JSON.
                let arguments = serde_json::from_str(&tc.function.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));
                ToolCall {
                    id: tc.id,
                    name: tc.function.name,
                    arguments,
                }
            })
            .collect();

        let finish_reason = match first.finish_reason.as_deref() {
            Some("stop") | None => FinishReason::Stop,
            Some("tool_calls") => FinishReason::ToolCalls,
            Some("length") => FinishReason::MaxTokens,
            _ => FinishReason::Stop,
        };

        let usage = parsed
            .usage
            .map(|u| Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            })
            .unwrap_or_default();

        Ok(ModelResponse {
            content,
            tool_calls,
            finish_reason,
            usage,
        })
    }
}

fn role_to_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

// Re-export for ergonomics — `messages` constructors used by other modules.
impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — pure construction; live HTTP tests are gated behind an env var
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctor_sets_fields() {
        let m = OpenAICompatModel::new("http://localhost:11434/v1", "qwen3");
        assert_eq!(m.name(), "qwen3");
        assert_eq!(m.base_url, "http://localhost:11434/v1");
        assert!(m.api_key.is_none());
    }

    #[test]
    fn with_api_key_attaches_key() {
        let m = OpenAICompatModel::new("http://x", "y").with_api_key("sk-test");
        assert_eq!(m.api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn message_constructors() {
        let s = Message::system("be helpful");
        assert!(matches!(s.role, Role::System));
        let u = Message::user("hi");
        assert!(matches!(u.role, Role::User));
        let a = Message::assistant("hello");
        assert!(matches!(a.role, Role::Assistant));
    }

    /// Live smoke test against whatever `MODEL_BASE_URL` / `MODEL_NAME`
    /// point to. Skipped unless `LIVE_MODEL_TEST=1` is set so CI without
    /// access to an inference backend stays green.
    #[tokio::test]
    async fn live_smoke() {
        if std::env::var("LIVE_MODEL_TEST").as_deref() != Ok("1") {
            return;
        }
        let m = OpenAICompatModel::from_env().expect("env config");
        let resp = m
            .complete(ModelRequest {
                messages: vec![Message::user("say 'ok' and nothing else.")],
                tools: vec![],
                temperature: 0.0,
                max_tokens: Some(8),
                stop: vec![],
            })
            .await
            .expect("model call");
        assert!(!resp.content.trim().is_empty());
    }
}
