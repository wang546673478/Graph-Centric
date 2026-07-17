//! Model trait surface and concrete implementations.
//!
//! Per design principle #1 (model-agnostic), the harness only depends on
//! the `Model` trait. Concrete backends plug in here.

pub mod capabilities;
pub mod config;
pub mod cache;
pub mod anthropic;
pub mod streaming;

// S1 of the OpenAI -> Anthropic migration: provider-agnostic types.
// Private to crate. The harness's public surface (Message, Role,
// ModelRequest, ModelResponse, StreamDelta, ModelWithEvents) is the OLD
// shape that 50+ caller sites in agent/*, web/*, skills/*, bin/* depend on;
// AnthropicModel translates at the wire boundary. S5 (M3 reasoning) and
// any future Type migration will broaden this visibility.
pub(crate) mod types;

pub use capabilities::{ModelCapabilities, ReasoningField, ThinkingStyle};
pub use config::ModelConfig;
pub use streaming::ModelWithEvents;

use crate::error::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// v2 spec §5.8: optional side-channel metadata. Used by the
    /// model layer to attach `usage.total_tokens` etc. without
    /// changing the wire shape. `#[serde(default, skip_serializing_if = "HashMap::is_empty")]`
    /// keeps backward compatibility with old messages and JSON dumps.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

impl Default for Message {
    fn default() -> Self {
        Self {
            role: Role::User,
            content: String::new(),
            extra: std::collections::HashMap::new(),
        }
    }
}

impl Message {
    /// Construct a Message with empty `extra`. Use this from
    /// existing call sites that don't yet need the side-channel.
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            extra: std::collections::HashMap::new(),
        }
    }

    /// Convenience constructors for the four message roles. These were
    /// historically defined in `openai_compat.rs`; moved here during the
    /// OpenAI → Anthropic migration (S6 final cleanup) so callers in
    /// `agent/*`, `web/*`, `skills/*`, `bin/*` could keep working after the
    /// legacy OpenAI file was deleted.
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: Role::System, content: content.into(), extra: std::collections::HashMap::new() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: content.into(), extra: std::collections::HashMap::new() }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: content.into(), extra: std::collections::HashMap::new() }
    }
    pub fn tool(content: impl Into<String>) -> Self {
        Self { role: Role::Tool, content: content.into(), extra: std::collections::HashMap::new() }
    }
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

/// A chunk of streaming output — either a content delta, a tool_call
/// argument fragment, or a terminal done signal. The
/// `ToolCallArgument` variant carries one `function.arguments` fragment
/// for a specific tool call (indexed by `index`) so the WS layer can
/// surface live tool-call progress to the frontend, mirroring how
/// `Delta.content` surfaces live text.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum StreamDelta {
    Delta {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },
    /// Partial arguments for one tool_call. OpenAI streams these as
    /// `delta.tool_calls[i].function.arguments` fragments; we forward
    /// them so the frontend can show "agent is calling X with…" in real
    /// time instead of waiting for the final `Done`.
    ToolCallArgument {
        /// OpenAI-assigned index into the `tool_calls` array.
        index: usize,
        /// Tool call id (set on the first fragment for this index).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Tool name (set once the model emits the first `function.name`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Raw arguments fragment (not yet valid JSON until assembled).
        arguments_fragment: String,
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
        // Forward any tool_calls as assembled-arguments fragments (one
        // per tool_call) so the streaming path mirrors the non-streaming
        // path's behavior. The SSE-aware AnthropicModel::complete_stream
        // override (in `model::anthropic`) fires per-fragment events that
        // the downstream forwarder (in `model::streaming`) coalesces
        // into this shape.
        for (i, tc) in resp.tool_calls.iter().enumerate() {
            let _ = tx.send(StreamDelta::ToolCallArgument {
                index: i,
                id: Some(tc.id.clone()),
                name: Some(tc.name.clone()),
                arguments_fragment: tc.arguments.to_string(),
            });
        }
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

    /// StreamDelta::ToolCallArgument must serialize to a tagged enum
    /// shape the frontend can distinguish from `Delta`. This guards
    /// against accidentally losing the `type` tag in a refactor.
    #[test]
    fn stream_delta_tool_call_argument_serialization() {
        let d = StreamDelta::ToolCallArgument {
            index: 0,
            id: Some("call_1".into()),
            name: Some("propose_patch".into()),
            arguments_fragment: r#"{"patch":"#.into(),
        };
        let s = serde_json::to_string(&d).unwrap();
        assert!(s.contains(r#""type":"tool_call_argument""#), "got: {s}");
        assert!(s.contains(r#""index":0"#));
        assert!(s.contains(r#""name":"propose_patch""#));
    }
}
