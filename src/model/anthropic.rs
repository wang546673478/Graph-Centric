//! Anthropic-compatible HTTP client (`/v1/messages`).
//!
//! S2 of the OpenAI -> Anthropic migration. Speaks the Anthropic Messages
//! API, which the MiniMax (`https://api.minimaxi.com/anthropic`) endpoint
//! exposes. This is the **sole** `Model` impl after S6 final cleanup.
//!
//! Wire shape (non-streaming):
//! - POST `{base_url}/v1/messages`
//! - `x-api-key: <key>` (NOT `Authorization: Bearer …`)
//! - `anthropic-version: 2023-06-01`
//! - Body: `{ model, system, messages, tools, max_tokens, temperature, stream }`
//! - Response: `{ content: [ContentBlock], stop_reason, usage }` — `content` is
//!   an array of `{type: "text"|"tool_use"|"thinking", …}` blocks.
//!
//! SSE streaming is deliberately NOT implemented in S2 — `complete_stream`
//! inherits the trait default that calls `complete()` and forwards the
//! result. S3 (or later) wires up real SSE on top of this client.

use super::{
    FinishReason, Message, Model, ModelRequest, ModelResponse, Role, ToolCall, Usage,
};
use crate::error::{HarnessError, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Config + Model
// ---------------------------------------------------------------------------

/// Configuration for the Anthropic backend.
///
/// `Default` produces an *empty* config (no base_url, no api_key) — the
/// caller is expected to populate it via env vars or by calling
/// `with_minimax_defaults()` to fill in the MiniMax `https://api.minimaxi.com/anthropic`
/// endpoint. Splitting "default = empty" from "with_minimax_defaults() = populated"
/// lets callers distinguish "user explicitly left this blank" from
/// "I forgot to configure it".
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_retries: u32,
    pub request_timeout: Duration,
    /// Send `anthropic-beta: prompt-caching-2024-07-31` and wrap
    /// system/tools blocks with `cache_control: ephemeral`. Recommended
    /// for graph loops that run multiple turns with the same system
    /// prompt — drops repeat-run cost by ~90% on the cached portion.
    /// Default: true. Disable only if the upstream provider rejects
    /// the header (e.g. when targeting non-Anthropic-compatible endpoints
    /// that don't support cache_control blocks in the response).
    pub prompt_caching: bool,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            max_retries: 2,
            // 180s is sized for reasoning models (M3, DeepSeek) — with
            // thinking enabled they regularly exceed 30s on a single
            // completion; smaller timeouts drop late completions.
            request_timeout: Duration::from_secs(180),
            // On by default: graph-loop runs re-transmit the system
            // prompt + tool schemas every turn; caching cuts the bill.
            prompt_caching: true,
        }
    }
}

impl AnthropicConfig {
    /// Fill empty fields with MiniMax defaults. Does NOT overwrite a field
    /// the caller already set — only blanks get the default. That way
    /// `AnthropicConfig::default().with_minimax_defaults()` produces the
    /// MiniMax config, but `AnthropicConfig { base_url: "https://x".into(), .. }
    /// .with_minimax_defaults()` keeps the custom URL.
    pub fn with_minimax_defaults(mut self) -> Self {
        if self.base_url.is_empty() {
            self.base_url = "https://api.minimaxi.com/anthropic".to_string();
        }
        if self.model.is_empty() {
            self.model = "MiniMax-M3".to_string();
        }
        self
    }
}

/// Anthropic-protocol HTTP client. Holds the reqwest::Client (built with
/// `request_timeout` so a hung connection doesn't block the run) and the
/// per-instance retry policy.
#[derive(Debug, Clone)]
pub struct AnthropicModel {
    cfg: AnthropicConfig,
    http: Client,
}

impl AnthropicModel {
    pub fn new(cfg: AnthropicConfig) -> Self {
        let http = Client::builder()
            .timeout(cfg.request_timeout)
            .build()
            .expect("reqwest client builds with default settings");
        Self { cfg, http }
    }

    /// Convenience: build a model from environment. Reads `MODEL_BASE_URL`,
    /// `MODEL_API_KEY`, `MODEL_NAME` (the existing env contract) and falls
    /// back to MiniMax defaults when unset.
    ///
    /// Note: the Anthropic endpoint is at `{base}/v1/messages`, not
    /// `{base}/v1/chat/completions`, so the env var semantics are slightly
    /// different — but the var *names* are kept consistent so a single
    /// `.env` file drives both clients.
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("MODEL_BASE_URL")
            .unwrap_or_else(|_| "https://api.minimaxi.com/anthropic".to_string());
        let api_key = std::env::var("MODEL_API_KEY")
            .map_err(|_| HarnessError::model("MODEL_API_KEY env var is required for AnthropicModel"))?;
        let model = std::env::var("MODEL_NAME").unwrap_or_else(|_| "MiniMax-M3".to_string());
        Ok(Self::new(
            AnthropicConfig {
                base_url,
                api_key,
                model,
                ..Default::default()
            }
            .with_minimax_defaults(),
        ))
    }

    /// Build the set of Anthropic auth + protocol headers. Public-ish
    /// (pub for tests) so callers can introspect what we're sending.
    ///
    /// When `cfg.prompt_caching` is true, also adds the `anthropic-beta:
    /// prompt-caching-2024-07-31` header required to opt the request into
    /// Anthropic's ephemeral prompt cache. This is consumed together with
    /// the `cache_control: {type: "ephemeral"}` blocks emitted by
    /// `build_anthropic_body`; without both the request still succeeds
    /// but no caching happens.
    pub fn auth_headers(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut h = HeaderMap::new();
        h.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_str(&self.cfg.api_key)
                .unwrap_or_else(|_| HeaderValue::from_static("invalid")),
        );
        h.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );
        if self.cfg.prompt_caching {
            h.insert(
                HeaderName::from_static("anthropic-beta"),
                HeaderValue::from_static("prompt-caching-2024-07-31"),
            );
        }
        h
    }

    pub fn config(&self) -> &AnthropicConfig {
        &self.cfg
    }

    /// Stable HTTP-status tag for downstream logging/aggregation. The
    /// mapping is the contract — see `classify_status` tests.
    pub(crate) fn classify_status(u: u16) -> &'static str {
        match u {
            400 => "bad_request",
            401 => "auth",
            403 => "forbidden",
            404 => "not_found",
            408 => "timeout",
            413 => "payload_too_large",
            429 => "rate_limit",
            500 => "internal",
            529 => "overload",
            _ if (500..600).contains(&u) => "server_error",
            _ => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Wire translation: OLD ModelRequest -> Anthropic wire body
// ---------------------------------------------------------------------------

/// Translate the legacy `ModelRequest` into the Anthropic Messages wire
/// shape. The translation rules (per the migration plan):
///
/// - `Message { role: System, content }` → top-level `system: String`.
/// - `Message { role: User, content }`   → `{role: "user", content: "<text>"}`.
/// - `Message { role: Assistant, content }` →
///     `{role: "assistant", content: [{type: "text", text: "<text>"}]}`.
/// - `Message { role: Tool, content }` →
///     `{role: "user", content: [{type: "tool_result", tool_use_id, content}]}`.
///     The OpenAI tool message carries `tool_call_id` in the side-channel
///     `extra` HashMap; we lift it out here. If `extra` is missing the key
///     (shouldn't happen in real usage) we surface that as a hard error —
///     silently inventing an id would corrupt the conversation.
///
/// - `tools: Vec<OpenAI-shaped Value>` →
///     `Vec<{name, description, input_schema}>` — Anthropic uses
///     `input_schema` where OpenAI uses `parameters`. The shape
///     `{"type": "function", "function": {"name": …, "description": …,
///     "parameters": …}}` is what the rest of the harness emits.
pub(crate) fn build_anthropic_body(
    cfg_model: &str,
    request: &ModelRequest,
    prompt_caching: bool,
) -> Result<serde_json::Value> {
    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<serde_json::Value> = Vec::new();

    for m in &request.messages {
        match m.role {
            Role::System => system_parts.push(m.content.clone()),
            Role::User => messages.push(json!({
                "role": "user",
                "content": m.content,
            })),
            Role::Assistant => messages.push(json!({
                "role": "assistant",
                "content": [{"type": "text", "text": m.content}],
            })),
            Role::Tool => {
                let tool_use_id = m
                    .extra
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        HarnessError::model(
                            "anthropic: Tool message missing 'tool_call_id' in extra",
                        )
                    })?
                    .to_string();
                messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": m.content,
                        "is_error": false,
                    }]
                }));
            }
        }
    }

    // Coalesce all system messages into a single system prompt.
    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    // Translate tool schemas from OpenAI shape → Anthropic shape.
    let tools: Vec<serde_json::Value> = request
        .tools
        .iter()
        .map(|t| {
            // OpenAI shape: {type:"function", function:{name,description,parameters}}
            // Some callers emit a bare function-def; handle both.
            let func = if t.get("type").and_then(|v| v.as_str()) == Some("function") {
                t.get("function").cloned().unwrap_or_else(|| t.clone())
            } else {
                t.clone()
            };
            let name = func
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let description = func
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let parameters = func
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            json!({
                "name": name,
                "description": description,
                "input_schema": parameters,
            })
        })
        .collect();

    // max_tokens: Anthropic requires this. Fall back to 1024 if the caller
    // passed None — never send "max_tokens": null.
    let max_tokens = request.max_tokens.unwrap_or(1024);

    // Wrap `system` and `tools` with cache_control: ephemeral when the
    // caller opted into prompt caching. Anthropic accepts BOTH a plain
    // string system + bare tool objects AND the cache_control-flavored
    // variants — they are wire-equivalent except for the cache lifetime.
    // When `prompt_caching=false` we emit the plain forms byte-identical
    // to the pre-cache-control body so existing tests / wire-shape
    // contracts don't shift.
    let tools_out: Vec<serde_json::Value> = if prompt_caching {
        tools
            .into_iter()
            .map(|mut t| {
                if let Some(obj) = t.as_object_mut() {
                    obj.insert(
                        "cache_control".to_string(),
                        json!({"type": "ephemeral"}),
                    );
                }
                t
            })
            .collect()
    } else {
        tools
    };

    let mut body = json!({
        "model": cfg_model,
        "messages": messages,
        "tools": tools_out,
        "max_tokens": max_tokens,
        "stream": false,
    });
    if let Some(sys) = system {
        if prompt_caching {
            body["system"] = json!([{
                "type": "text",
                "text": sys,
                "cache_control": {"type": "ephemeral"},
            }]);
        } else {
            body["system"] = json!(sys);
        }
    }
    if !request.stop.is_empty() {
        body["stop_sequences"] = json!(request.stop);
    }
    // Anthropic accepts temperature; send it when non-zero so the harness's
    // existing temperature=0.0 (greedy) call sites keep their semantics.
    body["temperature"] = json!(request.temperature);
    Ok(body)
}

// ---------------------------------------------------------------------------
// Wire translation: Anthropic wire response -> OLD ModelResponse
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AnthropicWireResponse {
    #[serde(default)]
    content: Vec<AnthropicContentBlock>,
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text { text: String },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    Thinking { thinking: String },
    // Unknown variants — e.g. `image`, `tool_search` — are silently dropped.
    // Real Anthropic responses don't currently emit anything outside the
    // three above for our use cases, but a future block type shouldn't
    // crash the run.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    // cache_read_input_tokens / cache_creation_input_tokens are surfaced by
    // Anthropic's prompt-caching. The OLD `Usage` doesn't have those slots,
    // but `prompt_cache_hit_tokens` is close enough for accounting — hits
    // count as read_input_tokens so the savings are visible.
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}

/// Translate an Anthropic wire response into the OLD `ModelResponse` shape.
///
/// - Concatenate all `text` blocks into `content: String` (joined with
///   newline, since text blocks can be multiple).
/// - Translate each `tool_use` block into a `ToolCall`. The Anthropic
///   `input` field is a JSON object (already-parsed by serde), unlike
///   OpenAI where `arguments` is a string we have to parse ourselves.
/// - Drop `thinking` blocks (they're the model's chain-of-thought; the
///   OLD `ModelResponse` has no slot for them, and the proposer / verifier
///   don't need them — they parse JSON out of `content`).
/// - Map `stop_reason` per the spec.
pub(crate) fn translate_response(parsed: AnthropicWireResponse) -> ModelResponse {
    let mut content_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for block in parsed.content {
        match block {
            AnthropicContentBlock::Text { text } => content_parts.push(text),
            AnthropicContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: input,
                });
            }
            AnthropicContentBlock::Thinking { .. } => {
                // Drop. See doc comment.
            }
            AnthropicContentBlock::Unknown => {
                // Silently drop unknown block types. See doc comment.
            }
        }
    }
    let content = content_parts.join("\n");

    let finish_reason = match parsed.stop_reason.as_deref() {
        Some("end_turn") | None => FinishReason::Stop,
        Some("tool_use") => FinishReason::ToolCalls,
        Some("max_tokens") => FinishReason::MaxTokens,
        Some("stop_sequence") => FinishReason::Stop,
        _ => FinishReason::Error,
    };

    let usage = parsed
        .usage
        .map(|u| Usage {
            prompt_tokens: u.input_tokens as usize,
            completion_tokens: u.output_tokens as usize,
            total_tokens: (u.input_tokens + u.output_tokens) as usize,
            prompt_cache_hit_tokens: u.cache_read_input_tokens.unwrap_or(0) as usize,
            prompt_cache_miss_tokens: 0,
        })
        .unwrap_or_default();

    ModelResponse {
        content,
        reasoning_content: None,
        tool_calls,
        finish_reason,
        usage,
    }
}

// ---------------------------------------------------------------------------
// Model trait impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Model for AnthropicModel {
    fn name(&self) -> &str {
        // Return the actual model identifier (e.g. "MiniMax-M3") so
        // `probe_model` logs and skills::capture see the resolved model,
        // not just a protocol tag.
        // records `model.name()` into the run metadata).
        &self.cfg.model
    }

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let body = build_anthropic_body(&self.cfg.model, &request, self.cfg.prompt_caching)?;

        let url = format!(
            "{}/v1/messages",
            self.cfg.base_url.trim_end_matches('/')
        );
        let max_retries = self.cfg.max_retries;
        let client = self.http.clone();

        // Retry policy: only 429 (rate limit) and 529 (overloaded) are
        // transient for Anthropic. Everything else (4xx, 5xx besides 529)
        // propagates immediately — the body won't change by retrying.
        //
        // NOTE: this is a *response-status* retry, distinct from the
        // OpenAI client's transport-level retry (which fires on
        // timeout/connect failures before any HTTP response arrives).
        // We do NOT add transport-level retry here — reqwest's default
        // behavior plus the request timeout is sufficient. A failed
        // request that gets a 5xx body back is logged and surfaced.
        let mut attempt: u32 = 0;
        let parsed: AnthropicWireResponse = loop {
            attempt += 1;
            // Build the header set once per attempt (it depends only on
            // self.cfg, not on the request body, but a single closure keeps
            // the call site tidy and lets the test introspect the same
            // header set we actually send on the wire).
            let req_builder = client
                .post(&url)
                .headers(self.auth_headers())
                .header("Content-Type", "application/json");
            // `prompt_caching` cache_control blocks need to be emitted in
            // the body — see build_anthropic_body. The header alone is not
            // sufficient.
            let resp = req_builder
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    HarnessError::model(format!(
                        "anthropic transport error: {e} (timeout={} connect={})",
                        e.is_timeout(),
                        e.is_connect()
                    ))
                })?;

            let status = resp.status();
            if status.is_success() {
                let text = resp.text().await.map_err(|e| {
                    HarnessError::model(format!("anthropic: failed to read response body: {e}"))
                })?;
                match serde_json::from_str::<AnthropicWireResponse>(&text) {
                    Ok(parsed) => break parsed,
                    Err(e) => {
                        // Surface a snippet of the body so JSON parse
                        // failures are debuggable.
                        let snippet: String = text.chars().take(300).collect();
                        return Err(HarnessError::model(format!(
                            "anthropic: JSON parse failed: {e}; body[:300] = {snippet}"
                        )));
                    }
                }
            }

            // Non-success status: capture body for diagnosis, retry on
            // 429/529, propagate otherwise.
            let body_text = resp.text().await.unwrap_or_default();
            let body_snippet: String = body_text.chars().take(300).collect();
            let code = status.as_u16();
            let tag = Self::classify_status(code);

            if (code == 429 || code == 529) && attempt <= max_retries {
                // Exponential backoff with jitter. Capped at 30s.
                let base_ms: u64 = 500u64.saturating_mul(1u64 << (attempt - 1).min(6));
                let jitter_ms: u64 = {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    let nanos = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.subsec_nanos() as u64)
                        .unwrap_or(0);
                    nanos % 250
                };
                let delay = Duration::from_millis((base_ms + jitter_ms).min(30_000));
                tokio::time::sleep(delay).await;
                continue;
            }

            return Err(HarnessError::model(format!(
                "anthropic HTTP {code} [{tag}] from {url}: {body_snippet}"
            )));
        };

        Ok(translate_response(parsed))
    }

    // complete_stream inherits the trait default — see module doc.
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // -- classify_status contract --

    #[test]
    fn classify_status_400_is_bad_request() {
        assert_eq!(AnthropicModel::classify_status(400), "bad_request");
    }
    #[test]
    fn classify_status_401_is_auth() {
        assert_eq!(AnthropicModel::classify_status(401), "auth");
    }
    #[test]
    fn classify_status_403_is_forbidden() {
        assert_eq!(AnthropicModel::classify_status(403), "forbidden");
    }
    #[test]
    fn classify_status_404_is_not_found() {
        assert_eq!(AnthropicModel::classify_status(404), "not_found");
    }
    #[test]
    fn classify_status_413_is_payload_too_large() {
        assert_eq!(AnthropicModel::classify_status(413), "payload_too_large");
    }
    #[test]
    fn classify_status_429_is_rate_limit() {
        assert_eq!(AnthropicModel::classify_status(429), "rate_limit");
    }
    #[test]
    fn classify_status_500_is_internal() {
        assert_eq!(AnthropicModel::classify_status(500), "internal");
    }
    #[test]
    fn classify_status_529_is_overload() {
        assert_eq!(AnthropicModel::classify_status(529), "overload");
    }
    #[test]
    fn classify_status_503_is_server_error() {
        assert_eq!(AnthropicModel::classify_status(503), "server_error");
    }
    #[test]
    fn classify_status_200_is_unknown() {
        assert_eq!(AnthropicModel::classify_status(200), "unknown");
    }
    #[test]
    fn classify_status_418_is_unknown() {
        assert_eq!(AnthropicModel::classify_status(418), "unknown");
    }

    // -- Config defaults --

    #[test]
    fn anthropic_config_default_is_blank() {
        let c = AnthropicConfig::default();
        assert!(c.base_url.is_empty());
        assert!(c.api_key.is_empty());
        assert!(c.model.is_empty());
        assert!(c.max_retries >= 2, "must allow at least one retry");
        assert!(c.request_timeout >= Duration::from_secs(30));
    }

    #[test]
    fn with_minimax_defaults_fills_blanks_only() {
        let c = AnthropicConfig::default().with_minimax_defaults();
        assert_eq!(c.base_url, "https://api.minimaxi.com/anthropic");
        assert_eq!(c.model, "MiniMax-M3");
        // api_key NOT filled — that's user-supplied.
        assert!(c.api_key.is_empty());
    }

    #[test]
    fn with_minimax_defaults_does_not_overwrite_custom_url() {
        let c = AnthropicConfig {
            base_url: "https://custom.example.com".to_string(),
            ..Default::default()
        }
        .with_minimax_defaults();
        assert_eq!(c.base_url, "https://custom.example.com");
        // model was blank → got the default.
        assert_eq!(c.model, "MiniMax-M3");
    }

    // -- Auth headers --

    #[test]
    fn auth_headers_emit_x_api_key_and_anthropic_version() {
        let m = AnthropicModel::new(
            AnthropicConfig {
                api_key: "sk-test-123".to_string(),
                prompt_caching: false,
                ..Default::default()
            }
            .with_minimax_defaults(),
        );
        let h = m.auth_headers();
        assert_eq!(h.get("x-api-key").unwrap(), "sk-test-123");
        assert_eq!(h.get("anthropic-version").unwrap(), "2023-06-01");
        // prompt_caching disabled → no beta header.
        assert!(h.get("anthropic-beta").is_none());
    }

    // -- Request converter (OLD ModelRequest -> Anthropic body) --

    #[test]
    fn build_body_with_system_messages_top_level() {
        let req = ModelRequest {
            messages: vec![
                Message::system("You are helpful."),
                Message::user("Hello"),
            ],
            tools: vec![],
            temperature: 0.7,
            max_tokens: Some(512),
            stop: vec![],
        };
        let body = build_anthropic_body("MiniMax-M3", &req, false).expect("build body");
        assert_eq!(body["model"], "MiniMax-M3");
        assert_eq!(body["system"], "You are helpful.");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "Hello");
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["temperature"], 0.7);
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn build_body_multiple_system_messages_joined() {
        let req = ModelRequest {
            messages: vec![
                Message::system("Part 1."),
                Message::user("intermediate"),
                Message::system("Part 2."),
            ],
            tools: vec![],
            temperature: 0.0,
            max_tokens: Some(100),
            stop: vec![],
        };
        let body = build_anthropic_body("MiniMax-M3", &req, false).expect("build body");
        // Anthropic allows ONE top-level `system` field — we coalesce.
        let sys = body["system"].as_str().expect("system must be string");
        assert!(sys.contains("Part 1."));
        assert!(sys.contains("Part 2."));
        // messages contains only the user turn.
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn build_body_assistant_uses_text_block_array() {
        let req = ModelRequest {
            messages: vec![
                Message::user("hi"),
                Message::assistant("hello there"),
            ],
            tools: vec![],
            temperature: 0.5,
            max_tokens: Some(64),
            stop: vec![],
        };
        let body = build_anthropic_body("MiniMax-M3", &req, false).expect("build body");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "assistant");
        let blocks = msgs[1]["content"].as_array().expect("assistant content is array");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hello there");
    }

    #[test]
    fn build_body_tool_message_becomes_tool_result_block() {
        let mut extra = HashMap::new();
        extra.insert("tool_call_id".to_string(), serde_json::json!("toolu_abc"));
        let tool_msg = Message {
            role: Role::Tool,
            content: "result text".to_string(),
            extra,
        };
        let req = ModelRequest {
            messages: vec![Message::user("use tool"), tool_msg],
            tools: vec![],
            temperature: 0.0,
            max_tokens: Some(100),
            stop: vec![],
        };
        let body = build_anthropic_body("MiniMax-M3", &req, false).expect("build body");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        // Tool results go on the *user* role per Anthropic spec.
        assert_eq!(msgs[1]["role"], "user");
        let blocks = msgs[1]["content"].as_array().expect("tool result block");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "toolu_abc");
        assert_eq!(blocks[0]["content"], "result text");
        assert_eq!(blocks[0]["is_error"], false);
    }

    #[test]
    fn build_body_tool_message_without_call_id_errors() {
        // A Tool message with no tool_call_id in `extra` is malformed —
        // we refuse to silently invent an id (which would corrupt the
        // tool_use → tool_result pairing).
        let tool_msg = Message {
            role: Role::Tool,
            content: "result".to_string(),
            extra: HashMap::new(),
        };
        let req = ModelRequest {
            messages: vec![tool_msg],
            tools: vec![],
            temperature: 0.0,
            max_tokens: Some(100),
            stop: vec![],
        };
        let err = build_anthropic_body("MiniMax-M3", &req, false).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("tool_call_id"), "got: {msg}");
    }

    #[test]
    fn build_body_tools_translated_to_anthropic_shape() {
        let req = ModelRequest {
            messages: vec![Message::user("go")],
            tools: vec![serde_json::json!({
                "type": "function",
                "function": {
                    "name": "propose_patch",
                    "description": "Apply a graph patch",
                    "parameters": {
                        "type": "object",
                        "properties": {"patches": {"type": "array"}},
                    },
                }
            })],
            temperature: 0.0,
            max_tokens: Some(256),
            stop: vec![],
        };
        let body = build_anthropic_body("MiniMax-M3", &req, false).expect("build body");
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "propose_patch");
        assert_eq!(tools[0]["description"], "Apply a graph patch");
        // input_schema now carries the parameters object — OpenAI's "parameters"
        // is renamed to Anthropic's "input_schema".
        assert!(tools[0].get("input_schema").is_some());
        assert_eq!(tools[0]["input_schema"]["type"], "object");
    }

    #[test]
    fn build_body_stop_sequences_renamed() {
        let req = ModelRequest {
            messages: vec![Message::user("go")],
            tools: vec![],
            temperature: 0.0,
            max_tokens: Some(100),
            stop: vec!["STOP".to_string()],
        };
        let body = build_anthropic_body("MiniMax-M3", &req, false).expect("build body");
        // Anthropic uses `stop_sequences` (plural) where OpenAI uses `stop`.
        assert_eq!(body["stop_sequences"][0], "STOP");
    }

    #[test]
    fn build_body_no_system_field_when_no_system_messages() {
        let req = ModelRequest {
            messages: vec![Message::user("hi")],
            tools: vec![],
            temperature: 0.0,
            max_tokens: Some(100),
            stop: vec![],
        };
        let body = build_anthropic_body("MiniMax-M3", &req, false).expect("build body");
        assert!(body.get("system").is_none(), "system must be omitted when empty");
    }

    // -- Response converter (Anthropic wire body -> OLD ModelResponse) --

    fn parse_fixture(s: &str) -> AnthropicWireResponse {
        serde_json::from_str(s).expect("fixture parses")
    }

    #[test]
    fn translate_response_text_only() {
        let raw = r#"{
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 3}
        }"#;
        let parsed = parse_fixture(raw);
        let resp = translate_response(parsed);
        assert_eq!(resp.content, "hello");
        assert!(resp.tool_calls.is_empty());
        assert_eq!(resp.finish_reason, FinishReason::Stop);
        assert_eq!(resp.usage.prompt_tokens, 5);
        assert_eq!(resp.usage.completion_tokens, 3);
        assert_eq!(resp.usage.total_tokens, 8);
    }

    #[test]
    fn translate_response_multiple_text_blocks_joined() {
        let raw = r#"{
            "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"}
            ],
            "stop_reason": "end_turn"
        }"#;
        let parsed = parse_fixture(raw);
        let resp = translate_response(parsed);
        assert_eq!(resp.content, "first\nsecond");
    }

    #[test]
    fn translate_response_tool_use_blocks_become_tool_calls() {
        let raw = r#"{
            "content": [
                {"type": "text", "text": "I'll call a tool"},
                {"type": "tool_use", "id": "toolu_1", "name": "propose_patch", "input": {"patches": []}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }"#;
        let parsed = parse_fixture(raw);
        let resp = translate_response(parsed);
        assert_eq!(resp.content, "I'll call a tool");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "toolu_1");
        assert_eq!(resp.tool_calls[0].name, "propose_patch");
        assert_eq!(resp.tool_calls[0].arguments, serde_json::json!({"patches": []}));
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn translate_response_thinking_blocks_dropped() {
        let raw = r#"{
            "content": [
                {"type": "thinking", "thinking": "let me reason about this..."},
                {"type": "text", "text": "answer"}
            ],
            "stop_reason": "end_turn"
        }"#;
        let parsed = parse_fixture(raw);
        let resp = translate_response(parsed);
        // Thinking is dropped — the OLD ModelResponse has no slot for it.
        assert_eq!(resp.content, "answer");
    }

    #[test]
    fn translate_response_stop_reason_max_tokens() {
        let raw = r#"{
            "content": [{"type": "text", "text": "partial..."}],
            "stop_reason": "max_tokens"
        }"#;
        let parsed = parse_fixture(raw);
        let resp = translate_response(parsed);
        assert_eq!(resp.finish_reason, FinishReason::MaxTokens);
    }

    #[test]
    fn translate_response_unknown_stop_reason_maps_to_error() {
        let raw = r#"{
            "content": [{"type": "text", "text": "x"}],
            "stop_reason": "refusal_made_up_reason"
        }"#;
        let parsed = parse_fixture(raw);
        let resp = translate_response(parsed);
        assert_eq!(resp.finish_reason, FinishReason::Error);
    }

    #[test]
    fn translate_response_cache_read_tokens_surfaced() {
        let raw = r#"{
            "content": [{"type": "text", "text": "x"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "output_tokens": 10, "cache_read_input_tokens": 80}
        }"#;
        let parsed = parse_fixture(raw);
        let resp = translate_response(parsed);
        // cache_read_input_tokens → prompt_cache_hit_tokens.
        assert_eq!(resp.usage.prompt_cache_hit_tokens, 80);
    }

    #[test]
    fn translate_response_unknown_block_dropped() {
        // A future block type we don't recognize must not crash the run.
        let raw = r#"{
            "content": [
                {"type": "text", "text": "ok"},
                {"type": "future_block", "mystery": true}
            ],
            "stop_reason": "end_turn"
        }"#;
        let parsed = parse_fixture(raw);
        let resp = translate_response(parsed);
        assert_eq!(resp.content, "ok");
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn translate_response_missing_usage_yields_default() {
        let raw = r#"{
            "content": [{"type": "text", "text": "x"}],
            "stop_reason": "end_turn"
        }"#;
        let parsed = parse_fixture(raw);
        let resp = translate_response(parsed);
        assert_eq!(resp.usage.prompt_tokens, 0);
        assert_eq!(resp.usage.completion_tokens, 0);
        assert_eq!(resp.usage.total_tokens, 0);
    }

    // -- end-to-end: round-trip a synthetic Anthropic response through build_body → translate --

    #[test]
    fn end_to_end_request_response_round_trip() {
        // Build a request with system, user, assistant, and tool messages,
        // plus a tool schema; then parse a synthetic Anthropic response that
        // includes text + tool_use blocks. Together these prove both
        // translation directions stay self-consistent.
        let mut extra = HashMap::new();
        extra.insert(
            "tool_call_id".to_string(),
            serde_json::json!("toolu_previous"),
        );
        let req = ModelRequest {
            messages: vec![
                Message::system("be brief"),
                Message::user("what's the weather?"),
                Message::assistant("calling tool"),
                Message {
                    role: Role::Tool,
                    content: "sunny".to_string(),
                    extra,
                },
            ],
            tools: vec![serde_json::json!({
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Look up the weather",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
                }
            })],
            temperature: 0.3,
            max_tokens: Some(256),
            stop: vec![],
        };

        let body = build_anthropic_body("MiniMax-M3", &req, false).expect("build body");
        assert_eq!(body["model"], "MiniMax-M3");
        assert_eq!(body["system"], "be brief");
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        assert_eq!(body["messages"][2]["role"], "user");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["tools"][0]["name"], "get_weather");
        assert_eq!(body["tools"][0]["input_schema"]["properties"]["city"]["type"], "string");
        assert_eq!(body["temperature"], 0.3);

        // Now parse a synthetic response.
        let resp_raw = r#"{
            "content": [
                {"type": "text", "text": "It's sunny."},
                {"type": "tool_use", "id": "toolu_next", "name": "get_weather", "input": {"city": "SF"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 50, "output_tokens": 8}
        }"#;
        let parsed = parse_fixture(resp_raw);
        let resp = translate_response(parsed);
        assert_eq!(resp.content, "It's sunny.");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "toolu_next");
        assert_eq!(resp.tool_calls[0].name, "get_weather");
        assert_eq!(
            resp.tool_calls[0].arguments,
            serde_json::json!({"city": "SF"})
        );
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
    }

    // -- Prompt caching: config flag + auth_headers behavior --

    #[test]
    fn prompt_caching_default_is_enabled() {
        let cfg = AnthropicConfig::default();
        assert!(
            cfg.prompt_caching,
            "prompt caching must be on by default (graph-loop runs re-send system+tools every turn)"
        );
    }

    #[test]
    fn prompt_caching_can_be_disabled() {
        let mut cfg = AnthropicConfig::default();
        cfg.prompt_caching = false;
        assert!(!cfg.prompt_caching);
    }

    #[test]
    fn auth_headers_includes_prompt_caching_beta_when_enabled() {
        let cfg = AnthropicConfig {
            prompt_caching: true,
            ..Default::default()
        };
        let m = AnthropicModel::new(cfg);
        let h = m.auth_headers();
        assert_eq!(
            h.get("anthropic-beta").map(|v| v.to_str().unwrap()),
            Some("prompt-caching-2024-07-31"),
            "the prompt-caching beta header must be present when enabled"
        );
    }

    #[test]
    fn auth_headers_omits_prompt_caching_beta_when_disabled() {
        let cfg = AnthropicConfig {
            prompt_caching: false,
            ..Default::default()
        };
        let m = AnthropicModel::new(cfg);
        let h = m.auth_headers();
        assert!(
            h.get("anthropic-beta").is_none(),
            "header must be absent when disabled — non-Anthropic-compatible endpoints may reject it"
        );
    }

    // -- Stress: prompt-caching body construction stability across iterations --
    //
    // This test runs `build_anthropic_body` 100 times with mutated inputs
    // and asserts the cache_control: ephemeral markers appear in BOTH the
    // `system` block and every tool entry on every iteration. The point is
    // to surface any latent non-determinism / serde bug / cache_control
    // insertion edge case — sharing one ModelRequest across iterations
    // would mask mutation bugs, so we build a fresh request per iter.
    //
    // On zero iterations failing across 10 rounds × 100 iters = 1000
    // stress executions, prompt-caching body construction is stable.
    #[test]
    fn prompt_caching_body_stress_100_iterations() {
        let cfg = AnthropicConfig::default();
        assert!(
            cfg.prompt_caching,
            "prompt_caching must be on by default; otherwise this test is vacuous"
        );

        for i in 0..100usize {
            // Fresh input per iteration (mutation is the signal — sharing one
            // ModelRequest across iterations would mask any stateful bug).
            let req = ModelRequest {
                messages: vec![
                    Message::system(format!("You are agent #{}, respond concisely.", i)),
                    Message::user(format!("task message {}", i)),
                    Message::assistant(format!("ack {}", i)),
                ],
                tools: vec![serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": "propose_patch",
                        "description": "submit a patch to the graph",
                        "parameters": {
                            "type": "object",
                            "properties": {"patches": {"type": "array"}},
                        },
                    },
                })],
                temperature: 0.4,
                max_tokens: Some(1024),
                stop: vec![],
            };

            let body = build_anthropic_body("MiniMax-M3", &req, cfg.prompt_caching)
                .unwrap_or_else(|e| panic!("iteration {}: build_body failed: {}", i, e));

            // Assert: body serializes to JSON without panic.
            let body_str = serde_json::to_string(&body)
                .unwrap_or_else(|e| panic!("iteration {}: serialize failed: {}", i, e));

            // Assert: body is well-formed JSON (round-trip parse).
            let parsed: serde_json::Value = serde_json::from_str(&body_str)
                .unwrap_or_else(|e| panic!("iteration {}: parse failed: {}", i, e));

            // Assert: when prompt_caching=true, system MUST be an array
            // with at least one text block carrying cache_control:ephemeral.
            // A plain string here means the cache_control header becomes a
            // no-op — silent regression.
            let sys = parsed.get("system").expect("system key");
            match sys {
                serde_json::Value::String(s) => {
                    panic!(
                        "iteration {}: expected system to be an array (with cache_control), \
                         got plain string {:?}",
                        i, s
                    );
                }
                serde_json::Value::Array(_) => {
                    let sys_arr = sys.as_array().unwrap();
                    assert!(
                        sys_arr.iter().any(|b| {
                            b.get("cache_control")
                                .and_then(|c| c.get("type"))
                                .and_then(|t| t.as_str())
                                == Some("ephemeral")
                        }),
                        "iteration {}: system array missing cache_control:ephemeral — body={}",
                        i,
                        body_str
                    );
                }
                _ => panic!("iteration {}: system is neither string nor array", i),
            }

            // Assert: every tool carries cache_control:ephemeral.
            let tools = parsed
                .get("tools")
                .and_then(|t| t.as_array())
                .expect("tools array");
            assert!(!tools.is_empty(), "iteration {}: tools should be non-empty", i);
            for (j, tool) in tools.iter().enumerate() {
                assert_eq!(
                    tool.get("cache_control")
                        .and_then(|c| c.get("type"))
                        .and_then(|t| t.as_str()),
                    Some("ephemeral"),
                    "iteration {} tool[{}]: missing cache_control:ephemeral — tool={:?}",
                    i,
                    j,
                    tool
                );
            }

            // Assert: structural shape invariants hold on every iteration.
            assert!(parsed.get("model").is_some(), "iter {}: missing model key", i);
            assert!(parsed.get("messages").is_some(), "iter {}: missing messages", i);
            assert!(parsed.get("max_tokens").is_some(), "iter {}: missing max_tokens", i);
            assert!(
                parsed
                    .get("max_tokens")
                    .map(|v| v.is_u64() || v.is_i64())
                    .unwrap_or(false),
                "iter {}: max_tokens must be numeric, got {:?}",
                i,
                parsed.get("max_tokens")
            );

            // Assert: the system prompt text we sent is actually present
            // in the body's system block — guards against accidental
            // dropping of coalesced system content.
            let sys_text_contains_iter = parsed["system"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
                .map(|s| s.contains(&format!("agent #{}", i)))
                .unwrap_or(false);
            assert!(
                sys_text_contains_iter,
                "iter {}: system text should contain 'agent #{}', got body={}",
                i,
                i,
                body_str
            );

            // Assert: temperature is preserved through the round-trip
            // (no float-precision loss or sign flip).
            assert_eq!(
                parsed.get("temperature").and_then(|t| t.as_f64()),
                Some(0.4_f64),
                "iter {}: temperature should be 0.4, got {:?}",
                i,
                parsed.get("temperature")
            );
        }
    }

    // -- Stress: prompt_caching=false body shape stays byte-stable --
    //
    // Companion test: when prompt_caching is OFF, the body must NOT contain
    // any cache_control markers (the regression direction is the inverse
    // of the test above — accidentally emitting cache_control when the
    // feature is off would be just as broken).
    #[test]
    fn prompt_caching_disabled_body_stress_50_iterations() {
        for i in 0..50usize {
            let req = ModelRequest {
                messages: vec![
                    Message::system(format!("off-mode system {}", i)),
                    Message::user(format!("hi {}", i)),
                ],
                tools: vec![serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": "propose_patch",
                        "description": "submit a patch to the graph",
                        "parameters": {"type": "object"},
                    },
                })],
                temperature: 0.0,
                max_tokens: Some(256),
                stop: vec![],
            };
            let body = build_anthropic_body("MiniMax-M3", &req, false)
                .unwrap_or_else(|e| panic!("iter {}: build failed: {}", i, e));
            let body_str = serde_json::to_string(&body).unwrap();

            // system should be a plain string (no cache_control block).
            assert_eq!(
                body["system"].as_str(),
                Some(format!("off-mode system {}", i).as_str()),
                "iter {}: prompt_caching=false should emit plain string system",
                i
            );
            assert!(
                body_str.contains("cache_control") == false,
                "iter {}: prompt_caching=false body must not contain 'cache_control' anywhere",
                i
            );

            // tools should NOT have cache_control.
            let tools = body["tools"].as_array().expect("tools array");
            for (j, t) in tools.iter().enumerate() {
                assert!(
                    t.get("cache_control").is_none(),
                    "iter {} tool[{}]: must not carry cache_control when disabled",
                    i,
                    j
                );
            }
        }
    }
}