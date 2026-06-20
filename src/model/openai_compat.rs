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
use super::capabilities::{ModelCapabilities, ReasoningField, ThinkingStyle};
use crate::error::{HarnessError, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::error::Error as _;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct OpenAICompatModel {
    pub base_url: String,
    pub model_name: String,
    pub api_key: Option<String>,
    client: Client,
    /// Per-model behavioral knobs (reasoning field, thinking style,
    /// temperature ceiling, token field). Inferred from `model_name` at
    /// construction; the agent core never sees these.
    pub capabilities: ModelCapabilities,
    /// Whether to request chain-of-thought ("thinking"). Default true,
    /// matching backends where omitting the toggle means thinking on.
    /// Only emitted when `capabilities.supports_thinking_toggle`.
    pub thinking_enabled: bool,
    /// DeepSeek-style reasoning effort ("high"/"max"). Only used when
    /// `thinking_style == DeepSeek`.
    pub reasoning_effort: Option<String>,
    /// Maximum number of attempts for a single model call (initial +
    /// retries). Transient HTTP errors (timeout, connect) trigger
    /// retries with exponential backoff up to this many attempts.
    pub retry_max_attempts: usize,
    /// Base delay before the first retry. Subsequent retries double
    /// this up to `retry_max_delay`.
    pub retry_base_delay: Duration,
    /// Cap on the exponential backoff so a long-running run doesn't
    /// sit on a single call indefinitely.
    pub retry_max_delay: Duration,
}

impl OpenAICompatModel {
    pub fn new(base_url: impl Into<String>, model_name: impl Into<String>) -> Self {
        // 180s timeout: reasoning models (MiniMax M3, DeepSeek) with thinking
        // enabled can take well over 30s for a single completion. The old 30s
        // cap caused spurious `operation timed out` errors mid-run.
        let client = Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .expect("reqwest client builds with default settings");
        let model_name = model_name.into();
        let capabilities = ModelCapabilities::from_model_name(&model_name);
        Self {
            base_url: base_url.into(),
            model_name,
            api_key: None,
            client,
            capabilities,
            thinking_enabled: true,
            reasoning_effort: None,
            retry_max_attempts: 1,
            retry_base_delay: Duration::from_secs(1),
            retry_max_delay: Duration::from_secs(30),
        }
    }

    /// Override the inferred capabilities (e.g. for a custom backend the
    /// name-based inference doesn't recognize).
    pub fn with_capabilities(mut self, caps: ModelCapabilities) -> Self {
        self.capabilities = caps;
        self
    }

    /// Configure thinking: whether it's on, and (DeepSeek) the effort level.
    pub fn with_thinking(mut self, enabled: bool, reasoning_effort: Option<String>) -> Self {
        self.thinking_enabled = enabled;
        self.reasoning_effort = reasoning_effort;
        self
    }

    fn build_request_body(&self, request: &ModelRequest) -> OpenAIChatRequest {
        let caps = &self.capabilities;
        let temperature = request.temperature.min(caps.temperature_max).max(0.0);
        // Token field: newer backends prefer max_completion_tokens.
        let (max_tokens, max_completion_tokens) = if caps.prefers_max_completion_tokens {
            (None, request.max_tokens)
        } else {
            (request.max_tokens, None)
        };
        // Thinking / reasoning fields, shaped per backend.
        let (thinking, reasoning_effort, reasoning_split) = self.thinking_fields();
        OpenAIChatRequest {
            model: self.model_name.clone(),
            messages: request.messages.iter().map(|m| OpenAIMessage {
                role: role_to_str(m.role).to_string(),
                content: m.content.clone(),
            }).collect(),
            temperature: Some(temperature),
            max_tokens,
            max_completion_tokens,
            stop: request.stop.clone(),
            tools: request.tools.clone(),
            stream: false,
            thinking,
            reasoning_effort,
            reasoning_split,
        }
    }

    /// Produce the (thinking, reasoning_effort, reasoning_split) request
    /// fields for this model's thinking style. All three are Option so
    /// they serialize only when relevant.
    fn thinking_fields(
        &self,
    ) -> (Option<serde_json::Value>, Option<String>, Option<bool>) {
        let caps = &self.capabilities;
        if !caps.supports_thinking_toggle {
            return (None, None, None);
        }
        match caps.thinking_style {
            ThinkingStyle::None => (None, None, None),
            ThinkingStyle::DeepSeek => {
                // DeepSeek toggles via reasoning_effort presence.
                let effort = if self.thinking_enabled {
                    Some(self.reasoning_effort.clone().unwrap_or_else(|| "high".into()))
                } else {
                    None
                };
                (None, effort, None)
            }
            ThinkingStyle::MiniMax => {
                // MiniMax M3: thinking object + reasoning_split so the
                // chain-of-thought returns in `reasoning_content` instead
                // of polluting `content` with <think> tags.
                let ty = if self.thinking_enabled { "adaptive" } else { "disabled" };
                (
                    Some(serde_json::json!({ "type": ty })),
                    None,
                    Some(true),
                )
            }
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Override the retry policy. Use this from tests that want a
    /// tight loop, or from callers that want a different budget.
    pub fn with_retry(
        mut self,
        max_attempts: usize,
        base_delay: Duration,
        max_delay: Duration,
    ) -> Self {
        self.retry_max_attempts = max_attempts;
        self.retry_base_delay = base_delay;
        self.retry_max_delay = max_delay;
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

#[derive(Debug, Clone, Serialize)]
struct OpenAIChatRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    /// MiniMax M3 thinking object `{"type":"adaptive"|"disabled"}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<serde_json::Value>,
    /// DeepSeek reasoning effort ("high"/"max").
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    /// MiniMax: route reasoning into `reasoning_content` instead of
    /// `<think>` tags inside `content`.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_split: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
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
    /// DeepSeek / MiniMax(reasoning_split) chain-of-thought channel.
    #[serde(default)]
    reasoning_content: Option<String>,
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
        let body = self.build_request_body(&request);

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let url_for_error = url.clone();
        let api_key = self.api_key.clone();
        let client = self.client.clone();

        // The send closure rebuilds the request on each retry so a
        // transient failure (e.g. TCP handshake hang, request timeout)
        // gets a fresh attempt. We do NOT retry on HTTP 4xx/5xx
        // responses — those are surfaced below with the response body
        // for diagnosis. We only retry on `is_transient_http_error`,
        // i.e. errors that come back before we have a response at all.
        let max_attempts = self.retry_max_attempts;
        let base_delay = self.retry_base_delay;
        let max_delay = self.retry_max_delay;
        let resp = send_with_retry(
            move || {
                let client = client.clone();
                let api_key = api_key.clone();
                let body = body.clone();
                let url = url.clone();
                async move {
                    let mut req = client.post(&url).json(&body);
                    if let Some(key) = &api_key {
                        req = req.bearer_auth(key);
                    }
                    req.send().await
                }
            },
            is_transient_http_error,
            max_attempts,
            base_delay,
            max_delay,
        )
        .await
        .map_err(|e| HarnessError::model(format_http_error("HTTP request failed", &e)))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(HarnessError::model(format!(
                "HTTP {status} from {url_for_error}: {body}"
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

        let raw_content = first.message.content.unwrap_or_default();
        // Reasoning channel: prefer the dedicated field; if the backend
        // returned <think> tags inline (MiniMax native / reasoning_split
        // off), split them out so `content` is clean for JSON parsing.
        let (content, reasoning_content) = match self.capabilities.reasoning_field {
            ReasoningField::ReasoningContent => {
                (raw_content, first.message.reasoning_content)
            }
            ReasoningField::ThinkTag => split_think_tags(&raw_content),
            ReasoningField::None => (raw_content, first.message.reasoning_content),
        };
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
                ..Default::default()
            })
            .unwrap_or_default();

        Ok(ModelResponse {
            content,
            reasoning_content,
            tool_calls,
            finish_reason,
            usage,
        })
    }

    /// Streaming implementation using SSE (`stream: true`).
    ///
    /// The initial connection is retried on transient errors (timeout/connect)
    /// with the same exponential backoff as `complete()`. Once the stream starts
    /// flowing, chunk errors are terminal — we don't retry mid-stream because
    /// partial data has already been sent to the caller.
    async fn complete_stream(
        &self,
        request: ModelRequest,
        tx: tokio::sync::mpsc::UnboundedSender<crate::model::StreamDelta>,
    ) -> crate::error::Result<ModelResponse> {
        use futures_util::StreamExt;

        let mut body = self.build_request_body(&request);
        body.stream = true;

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let url_for_error = url.clone();
        let api_key = self.api_key.clone();
        let client = self.client.clone();

        // Retry the initial connection (same policy as non-streaming `complete()`).
        let max_attempts = self.retry_max_attempts;
        let base_delay = self.retry_base_delay;
        let max_delay = self.retry_max_delay;
        let resp = send_with_retry(
            move || {
                let client = client.clone();
                let api_key = api_key.clone();
                let body = body.clone();
                let url = url.clone();
                async move {
                    let mut req = client
                        .post(&url)
                        .header("Content-Type", "application/json")
                        .json(&body);
                    if let Some(key) = &api_key {
                        req = req.header("Authorization", format!("Bearer {key}"));
                    }
                    req.send().await
                }
            },
            is_transient_http_error,
            max_attempts,
            base_delay,
            max_delay,
        )
        .await
        .map_err(|e| {
            crate::error::HarnessError::model(format_http_error("stream request failed", &e))
        })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::error::HarnessError::model(format!(
                "stream HTTP {} from {url_for_error}: {}",
                status.as_u16(),
                body.chars().take(300).collect::<String>(),
            )));
        }

        // Stream is flowing — no retry from here, but enforce a read timeout.
        let mut stream = resp.bytes_stream();
        let stream_start = std::time::Instant::now();
        let stream_timeout = std::time::Duration::from_secs(120);
        let mut full_content = String::new();
        let mut full_reasoning = String::new();
        let mut finish_reason = FinishReason::Stop;
        let mut usage = Usage::default();
        let mut buf = String::new();
        // Accumulate streaming tool_calls by index.
        // Key: tool_call index. Value: (id, name, arguments_json_fragment).
        let mut tool_call_buf: std::collections::HashMap<
            usize,
            (String, String, String),
        > = std::collections::HashMap::new();

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "stream chunk error");
                    break;
                }
            };
            // If we haven't received any data within the timeout, abort.
            if full_content.is_empty() && full_reasoning.is_empty()
                && stream_start.elapsed() > stream_timeout
            {
                return Err(crate::error::HarnessError::model(
                    "stream timed out: no content received within 120s"
                ));
            }
            buf.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE lines.
            while let Some(line_end) = buf.find('\n') {
                let line = buf[..line_end].trim().to_string();
                buf = buf[line_end + 1..].to_string();

                let data = line.strip_prefix("data: ").or_else(|| line.strip_prefix("data:"));
                let Some(data) = data else { continue };
                if data == "[DONE]" {
                    let _ = tx.send(crate::model::StreamDelta::Done {
                        finish_reason,
                        usage: usage.clone(),
                    });
                    // Break both loops — stream is finished.
                    return Ok(ModelResponse {
                        content: full_content,
                        reasoning_content: if full_reasoning.is_empty() {
                            None
                        } else {
                            Some(full_reasoning)
                        },
                        tool_calls: tool_calls_from_buf(&mut tool_call_buf),
                        finish_reason,
                        usage,
                    });
                }

                let Ok(chunk) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                if let Some(choices) = chunk["choices"].as_array() {
                    for choice in choices {
                        if let Some(delta) = choice.get("delta") {
                            // Accumulate streaming tool_calls.
                            if let Some(tc_array) = delta["tool_calls"].as_array() {
                                for tc in tc_array {
                                    let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                                    let entry = tool_call_buf
                                        .entry(idx)
                                        .or_insert_with(|| (String::new(), String::new(), String::new()));
                                    if let Some(id) = tc["id"].as_str() {
                                        entry.0 = id.to_string();
                                    }
                                    if let Some(func) = tc.get("function") {
                                        if let Some(name) = func["name"].as_str() {
                                            if !name.is_empty() {
                                                entry.1 = name.to_string();
                                            }
                                        }
                                        if let Some(args) = func["arguments"].as_str() {
                                            entry.2.push_str(args);
                                        }
                                    }
                                }
                            }
                            if let Some(content) = delta["content"].as_str() {
                                full_content.push_str(content);
                                let _ = tx.send(crate::model::StreamDelta::Delta {
                                    content: content.to_string(),
                                    reasoning_content: None,
                                });
                            }
                            if let Some(reasoning) = delta["reasoning_content"].as_str() {
                                full_reasoning.push_str(reasoning);
                                let _ = tx.send(crate::model::StreamDelta::Delta {
                                    content: String::new(),
                                    reasoning_content: Some(reasoning.to_string()),
                                });
                            }
                        }
                        if let Some(fr) = choice["finish_reason"].as_str() {
                            finish_reason = match fr {
                                "stop" => FinishReason::Stop,
                                "tool_calls" => FinishReason::ToolCalls,
                                "length" => FinishReason::MaxTokens,
                                _ => FinishReason::Stop,
                            };
                        }
                    }
                }
                if let Some(u) = chunk.get("usage") {
                    usage = Usage {
                        prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0) as usize,
                        completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0) as usize,
                        total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as usize,
                        prompt_cache_hit_tokens: u["prompt_cache_hit_tokens"].as_u64().unwrap_or(0) as usize,
                        prompt_cache_miss_tokens: u["prompt_cache_miss_tokens"].as_u64().unwrap_or(0) as usize,
                    };
                }
            }
        }

        // Stream ended without [DONE] — return what we have.
        let reasoning = if full_reasoning.is_empty() { None } else { Some(full_reasoning) };
        Ok(ModelResponse {
            content: full_content,
            reasoning_content: reasoning,
            tool_calls: tool_calls_from_buf(&mut tool_call_buf),
            finish_reason,
            usage,
        })
    }
}

/// Convert accumulated streaming tool_call fragments into Vec<ToolCall>.
fn tool_calls_from_buf(
    buf: &mut std::collections::HashMap<usize, (String, String, String)>,
) -> Vec<ToolCall> {
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut indices: Vec<usize> = buf.keys().copied().collect();
    indices.sort();
    for idx in indices {
        if let Some((id, name, arguments)) = buf.remove(&idx) {
            if name.is_empty() {
                continue;
            }
            let args = serde_json::from_str(&arguments)
                .unwrap_or_else(|_| serde_json::Value::String(arguments.clone()));
            tool_calls.push(ToolCall { id, name, arguments: args });
        }
    }
    tool_calls
}

fn role_to_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Split `<think>...</think>` reasoning out of an inline content string
/// (MiniMax native mode / reasoning_split off). Returns (clean_content,
/// reasoning). If no think tags are present, returns the content unchanged
/// with `None` reasoning. Handles a single leading think block, which is
/// the shape these backends produce.
fn split_think_tags(raw: &str) -> (String, Option<String>) {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    if let Some(open_idx) = raw.find(OPEN) {
        if let Some(close_rel) = raw[open_idx + OPEN.len()..].find(CLOSE) {
            let reasoning_start = open_idx + OPEN.len();
            let reasoning = raw[reasoning_start..reasoning_start + close_rel].trim().to_string();
            let mut clean = String::with_capacity(raw.len());
            clean.push_str(&raw[..open_idx]);
            clean.push_str(&raw[reasoning_start + close_rel + CLOSE.len()..]);
            let clean = clean.trim().to_string();
            let reasoning = if reasoning.is_empty() { None } else { Some(reasoning) };
            return (clean, reasoning);
        }
    }
    (raw.to_string(), None)
}

// ---------------------------------------------------------------------------
// Retry + error formatting
// ---------------------------------------------------------------------------

/// Classify a `reqwest::Error` as transient (worth retrying) or terminal.
///
/// Transient: timeouts and connection failures. The request never got a
/// response, so the network may have recovered by the next attempt.
///
/// Non-transient: body / decode / redirect errors. The request shape is
/// wrong, the response couldn't be parsed, or the server redirected —
/// none of those get better by retrying.
fn is_transient_http_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect()
}

/// Render a `reqwest::Error` with its full `std::error::Error` source
/// chain, so a single line of log surface DNS/TCP/TLS/IO causes
/// instead of just the reqwest-level summary. The default `Display`
/// impl stops at the top frame, which is why a 5-minute hang shows
/// up as the unhelpful "error sending request for url".
fn format_http_error(prefix: &str, e: &reqwest::Error) -> String {
    let mut s = format!(
        "{prefix}: {e} (timeout={} connect={} request={} body={} decode={})",
        e.is_timeout(),
        e.is_connect(),
        e.is_request(),
        e.is_body(),
        e.is_decode(),
    );
    let mut src: Option<&(dyn std::error::Error + 'static)> = e.source();
    let mut depth = 0;
    while let Some(cause) = src {
        if depth >= 5 {
            break;
        }
        s.push_str(&format!(" -> {cause}"));
        src = cause.source();
        depth += 1;
    }
    s
}

/// Generic retry helper.
///
/// Calls `send` until it returns `Ok`, the error is classified as
/// non-transient by `classify`, or `max_attempts` is reached. Between
/// attempts, sleeps for `base_delay` and doubles up to `max_delay`.
///
/// The error type `E` is arbitrary — `classify(&E) -> bool` decides
/// retry-worthiness. This keeps the helper testable with a mock error
/// type without needing to construct a real `reqwest::Error`.
async fn send_with_retry<T, E, F, Fut>(
    mut send: F,
    classify: impl Fn(&E) -> bool,
    max_attempts: usize,
    base_delay: Duration,
    max_delay: Duration,
) -> std::result::Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, E>>,
{
    assert!(max_attempts >= 1, "max_attempts must be >= 1");
    let mut attempt = 0usize;
    let mut delay = base_delay;
    loop {
        attempt += 1;
        match send().await {
            Ok(t) => return Ok(t),
            Err(e) => {
                if attempt >= max_attempts || !classify(&e) {
                    return Err(e);
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(max_delay);
            }
        }
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

    fn req(temperature: f64, max_tokens: Option<usize>) -> ModelRequest {
        ModelRequest {
            messages: vec![Message { role: Role::User, content: "hi".into() }],
            tools: vec![],
            temperature,
            max_tokens,
            stop: vec![],
        }
    }

    #[test]
    fn split_think_tags_extracts_reasoning() {
        let (clean, reasoning) =
            split_think_tags("<think>let me reason</think>{\"step\":\"x\"}");
        assert_eq!(clean, "{\"step\":\"x\"}");
        assert_eq!(reasoning.as_deref(), Some("let me reason"));
    }

    #[test]
    fn split_think_tags_passthrough_without_tags() {
        let (clean, reasoning) = split_think_tags("{\"step\":\"x\"}");
        assert_eq!(clean, "{\"step\":\"x\"}");
        assert!(reasoning.is_none());
    }

    #[test]
    fn minimax_request_uses_thinking_object_and_completion_tokens() {
        let m = OpenAICompatModel::new("https://api.minimaxi.com/v1", "MiniMax-M3");
        let body = m.build_request_body(&req(0.7, Some(1000)));
        // MiniMax prefers max_completion_tokens, not max_tokens.
        assert_eq!(body.max_completion_tokens, Some(1000));
        assert_eq!(body.max_tokens, None);
        // thinking object + reasoning_split present.
        assert!(body.thinking.is_some());
        assert_eq!(body.reasoning_split, Some(true));
        assert!(body.reasoning_effort.is_none());
    }

    #[test]
    fn minimax_thinking_disabled_sets_type_disabled() {
        let m = OpenAICompatModel::new("https://api.minimaxi.com/v1", "MiniMax-M3")
            .with_thinking(false, None);
        let body = m.build_request_body(&req(0.7, Some(1000)));
        assert_eq!(body.thinking, Some(serde_json::json!({"type": "disabled"})));
    }

    #[test]
    fn deepseek_request_uses_reasoning_effort() {
        let m = OpenAICompatModel::new("https://api.deepseek.com/v1", "deepseek-v4-pro")
            .with_thinking(true, Some("max".into()));
        let body = m.build_request_body(&req(0.5, Some(800)));
        assert_eq!(body.reasoning_effort.as_deref(), Some("max"));
        assert_eq!(body.max_tokens, Some(800)); // legacy field
        assert!(body.max_completion_tokens.is_none());
        assert!(body.thinking.is_none());
    }

    #[test]
    fn unknown_model_emits_no_thinking_fields() {
        let m = OpenAICompatModel::new("http://localhost:11434/v1", "llama3");
        let body = m.build_request_body(&req(0.7, Some(500)));
        assert!(body.thinking.is_none());
        assert!(body.reasoning_effort.is_none());
        assert!(body.reasoning_split.is_none());
    }

    #[test]
    fn temperature_clamped_to_capability_max() {
        let m = OpenAICompatModel::new("https://api.minimaxi.com/v1", "MiniMax-M3");
        let body = m.build_request_body(&req(5.0, None));
        assert_eq!(body.temperature, Some(2.0));
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

    // -----------------------------------------------------------------------
    // Retry helper — exercised with a mock error type so the unit test
    // does not need a real `reqwest::Error` (which has no public ctor).
    // -----------------------------------------------------------------------

    /// Stand-in for `reqwest::Error` that the retry helper can classify.
    /// `transient: true` mimics timeout/connect failures; `transient: false`
    /// mimics body/decode/redirect failures.
    #[derive(Debug, PartialEq)]
    struct MockHttpError {
        transient: bool,
        label: &'static str,
    }

    fn classify_mock(e: &MockHttpError) -> bool {
        e.transient
    }

    #[tokio::test]
    async fn send_with_retry_succeeds_on_first_try() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let result: std::result::Result<&'static str, MockHttpError> = send_with_retry(
            || {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Ok("ok") }
            },
            classify_mock,
            3,
            Duration::from_millis(1),
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn send_with_retry_recovers_after_transient_failures() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let result: std::result::Result<&'static str, MockHttpError> = send_with_retry(
            || {
                let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if n < 2 {
                        Err(MockHttpError {
                            transient: true,
                            label: "timeout",
                        })
                    } else {
                        Ok("ok")
                    }
                }
            },
            classify_mock,
            5,
            Duration::from_millis(1),
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(result.unwrap(), "ok");
        // Two failures, then success → three total attempts.
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn send_with_retry_gives_up_on_non_transient() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let result: std::result::Result<&'static str, MockHttpError> = send_with_retry(
            || {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async {
                    Err(MockHttpError {
                        transient: false,
                        label: "body",
                    })
                }
            },
            classify_mock,
            5,
            Duration::from_millis(1),
            Duration::from_millis(10),
        )
        .await;
        // Non-transient → no retry, first error returned verbatim.
        assert_eq!(
            result.unwrap_err(),
            MockHttpError {
                transient: false,
                label: "body"
            }
        );
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn send_with_retry_exhausts_max_attempts() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let result: std::result::Result<&'static str, MockHttpError> = send_with_retry(
            || {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async {
                    Err(MockHttpError {
                        transient: true,
                        label: "always-times-out",
                    })
                }
            },
            classify_mock,
            3,
            Duration::from_millis(1),
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(result.unwrap_err().label, "always-times-out");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn send_with_retry_applies_backoff_between_attempts() {
        // 3 transient failures, max_attempts=4 → 3 sleeps total. We use
        // generous base/max delays so the test is robust to CI jitter
        // but the *lower bound* still proves the sleep actually ran.
        let start = std::time::Instant::now();
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let _ = send_with_retry::<(), MockHttpError, _, _>(
            || {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async {
                    Err(MockHttpError {
                        transient: true,
                        label: "slow",
                    })
                }
            },
            classify_mock,
            4,
            Duration::from_millis(20),
            Duration::from_millis(100),
        )
        .await;
        let elapsed = start.elapsed();
        // Three sleeps: 20ms + 40ms + 80ms = 140ms minimum. Allow some
        // slack but assert we spent at least that long sleeping.
        assert!(
            elapsed >= Duration::from_millis(120),
            "elapsed {elapsed:?} is shorter than the sum of the three backoff sleeps"
        );
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn send_with_retry_caps_backoff_at_max_delay() {
        // base=20ms, doubled three times: 20, 40, 80, 160 → capped at 50ms.
        // Three sleeps then → 20 + 40 + 50 = 110ms minimum.
        let start = std::time::Instant::now();
        let _ = send_with_retry::<(), MockHttpError, _, _>(
            || async {
                Err(MockHttpError {
                    transient: true,
                    label: "slow",
                })
            },
            classify_mock,
            4,
            Duration::from_millis(20),
            Duration::from_millis(50),
        )
        .await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(90),
            "elapsed {elapsed:?} should reflect the capped backoff schedule"
        );
    }
}
