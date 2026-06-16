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
use std::error::Error as _;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct OpenAICompatModel {
    pub base_url: String,
    pub model_name: String,
    pub api_key: Option<String>,
    client: Client,
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
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("reqwest client builds with default settings");
        Self {
            base_url: base_url.into(),
            model_name: model_name.into(),
            api_key: None,
            client,
            retry_max_attempts: 3,
            retry_base_delay: Duration::from_secs(1),
            retry_max_delay: Duration::from_secs(30),
        }
    }

    fn build_request_body(&self, request: &ModelRequest) -> OpenAIChatRequest {
        OpenAIChatRequest {
            model: self.model_name.clone(),
            messages: request.messages.iter().map(|m| OpenAIMessage {
                role: role_to_str(m.role).to_string(),
                content: m.content.clone(),
            }).collect(),
            temperature: Some(request.temperature),
            max_tokens: request.max_tokens,
            stop: request.stop.clone(),
            tools: request.tools.clone(),
            stream: false,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
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
            stream: false,
        };

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
                ..Default::default()
            })
            .unwrap_or_default();

        Ok(ModelResponse {
            content,
            reasoning_content: None,
            tool_calls,
            finish_reason,
            usage,
        })
    }

    /// Streaming implementation using SSE (`stream: true`).
    async fn complete_stream(
        &self,
        request: ModelRequest,
        tx: tokio::sync::mpsc::UnboundedSender<crate::model::StreamDelta>,
    ) -> crate::error::Result<ModelResponse> {
        use futures_util::StreamExt;

        let mut body = self.build_request_body(&request);
        body.stream = true;

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body);
        if let Some(ref key) = self.api_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        let resp = req.send().await.map_err(|e| {
            crate::error::HarnessError::model(format!("stream request failed: {e}"))
        })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::error::HarnessError::model(format!(
                "stream HTTP {}: {}",
                status.as_u16(),
                body.chars().take(300).collect::<String>(),
            )));
        }

        let mut stream = resp.bytes_stream();
        let mut full_content = String::new();
        let mut full_reasoning = String::new();
        let mut finish_reason = FinishReason::Stop;
        let mut usage = Usage::default();
        let mut buf = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "stream chunk error");
                    break;
                }
            };
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
                    break;
                }

                let Ok(chunk) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                if let Some(choices) = chunk["choices"].as_array() {
                    for choice in choices {
                        if let Some(delta) = choice.get("delta") {
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

        let reasoning = if full_reasoning.is_empty() { None } else { Some(full_reasoning) };
        Ok(ModelResponse {
            content: full_content,
            reasoning_content: reasoning,
            tool_calls: Vec::new(),
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
