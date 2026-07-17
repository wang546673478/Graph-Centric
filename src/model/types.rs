//! Provider-agnostic model types.
//!
//! S1 of the OpenAI -> Anthropic migration introduces these types so that
//! callers (proposer, decomposer, enricher, verifier, reviewer, cascade,
//! subagent) and the Anthropic-specific client can speak the same shape.
//! The exact HTTP wire format belongs to the client implementations
//! (`openai_compat.rs`, S2's `anthropic.rs`); these types are the in-memory
//! representation that crosses the `Model` trait boundary.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------- Content blocks ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    /// S5 wires MiniMax-M3 reasoning into this variant. Empty in S1.
    Thinking { thinking: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentBlockInit {
    Text,
    Thinking,
    ToolUse { id: String, name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ContentBlockDelta {
    TextDelta(String),
    ThinkingDelta(String),
    InputJsonDelta(String), // partial JSON; client concatenates then parses
}

// ---------- Messages ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Role {
    #[default]
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ConversationMessage {
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
    /// For assistant turns that produced a tool call, the resulting `tool_result`
    /// blocks are appended on the next user turn. We keep it as a separate list
    /// rather than threading through `ContentBlock` so the user/assistant
    /// asymmetry matches Anthropic's `messages[].content` shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<ToolResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub content: serde_json::Value,
    #[serde(default)]
    pub is_error: bool,
}

// ---------- Tool schema ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema object. OpenAI historically used `parameters`; Anthropic uses
    /// `input_schema`. The field name here matches Anthropic; callers translate.
    pub input_schema: serde_json::Value,
}

// ---------- Request / response ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct ModelRequest {
    /// Anthropic takes system prompt as a top-level field, distinct from
    /// `messages`. S4 migrates callers off `messages[0]`-as-system.
    pub system: Option<String>,
    pub messages: Vec<ConversationMessage>,
    pub tools: Vec<ToolSchema>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop: Vec<String>,
    /// Extension hook for thinking/reasoning/anything Anthropic-specific.
    /// S5 fills this with `thinking` config.
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub id: String,
    pub model: String,
    pub blocks: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

// ---------- Streaming events (Anthropic-native shape) ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StreamDelta {
    /// Equivalent to Anthropic's `content_block_start`. `init` carries the
    /// block's identity (id/name for tool_use; nothing for text/thinking).
    ContentStart { index: u32, init: ContentBlockInit },
    /// Equivalent to Anthropic's `content_block_delta`.
    ContentDelta { index: u32, delta: ContentBlockDelta },
    /// Equivalent to Anthropic's `content_block_stop`.
    ContentStop { index: u32 },
    /// Equivalent to Anthropic's `message_delta` + `message_stop`.
    MessageEnd { stop_reason: StopReason, usage: Usage },
    /// Anthropic error events or transport-level failures.
    Error { code: Option<u32>, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_block_serde_round_trip_text() {
        let block = ContentBlock::Text { text: "hello".into() };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
    }

    #[test]
    fn content_block_serde_round_trip_tool_use() {
        let block = ContentBlock::ToolUse {
            id: "toolu_abc".into(),
            name: "propose_patch".into(),
            input: serde_json::json!({ "patches": [] }),
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
    }

    #[test]
    fn content_block_serde_round_trip_thinking() {
        let block = ContentBlock::Thinking { thinking: "step 1: ...".into() };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
    }

    #[test]
    fn content_block_delta_text_delta_serde() {
        let d = ContentBlockDelta::TextDelta("part".into());
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("text_delta"));
        let back: ContentBlockDelta = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn stream_delta_message_end_serde() {
        let d = StreamDelta::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: StreamDelta = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn stop_reason_json_values() {
        // Pin the wire-format strings so S2 can rely on them.
        assert_eq!(
            serde_json::to_string(&StopReason::EndTurn).unwrap(),
            "\"end_turn\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::MaxTokens).unwrap(),
            "\"max_tokens\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::ToolUse).unwrap(),
            "\"tool_use\""
        );
    }

    #[test]
    fn model_request_default_is_blank() {
        let r = ModelRequest::default();
        assert!(r.system.is_none());
        assert!(r.messages.is_empty());
        assert!(r.tools.is_empty());
        assert!(r.extra.is_empty());
    }
}
