# Anthropic Migration S1 — Model Trait Abstraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce provider-agnostic types (`ContentBlock`, `ModelRequest`, `ModelResponse`, `StreamDelta`) and the `AnthropicModel` type skeleton so S2 can plug in the HTTP client without further refactoring.

**Architecture:** Add a new `src/model/types.rs` containing all provider-agnostic types and their serde derives. Add a new `src/model/anthropic.rs` containing `AnthropicConfig`, `AnthropicModel` skeleton (stub `Model` impl with `todo!()` bodies — S2 fills these), and a small status-classifier helper. Update `src/model/mod.rs` to (a) re-export the new types, (b) redefine `Model` trait with new method signatures, (c) re-align `ModelWithEvents` to the new `StreamDelta`. Adapt `src/model/openai_compat.rs` to be a boundary adapter that translates between OpenAI wire shape and the new types so existing callers keep compiling untouched.

**Tech Stack:** Rust 2024 edition, `graph_harness` crate, async-trait, serde, reqwest, futures-util, anyhow. Edition is 2024 — modern Rust syntax (`async fn`, `let-else`, `if let Some(...) && ...` chains) is all available; no need to lean on `async-trait` for `async fn in trait`, but the existing crate uses `async-trait` macro for object-safe trait objects — keep that for consistency.

## Global Constraints

- **Crate name:** `graph_harness`. Verify with `grep '^name = ' Cargo.toml` before running test commands.
- **Test commands:** `cargo test --lib` (matches CLAUDE.md guidance).
- **Build:** `cargo build --bin serve` is the canonical check; will fail-fast on any compile error.
- **Baseline test count is 653 (652 pass + 1 pre-existing flake), NOT the "606" CLAUDE.md cites.** The 1 failing test is `agent::graph_loop::tests::validator_passed_lets_loop_proceed_to_review` at `src/agent/graph_loop.rs:6069` — a flaky LLM-shim fixture, **unrelated to the model layer**. Track pass-count delta, not absolute. A green S1 should show **652 + 12 new (types + anthropic) = 664 passing**, with the same 1 pre-existing failure.
- **Test pass/fail convergence is the metric, not the absolute count.** If S1 implementation flips a previously-passing test to failing, that's a regression regardless of baseline count.
- **Branch:** commit directly on `main` (project memory rule). No feature branches.
- **Push:** every commit followed by `git push origin main`. Network may hiccup — retry with `sleep 2-5` if "Connection closed" returned.
- **No caller changes in S1.** The 7 model layers (proposer/decomposer/enricher/verifier/reviewer/cascade/subagent) MUST compile unchanged after this plan lands. S4 is the dedicated sub-project for caller migration.
- **`openai_compat.rs` does not change its wire behavior in S1.** It still speaks the OpenAI protocol over HTTP; only the Rust types it deals with are reshaped. The wire format (HTTP requests, SSE event names) is preserved verbatim so production OpenAI usage continues to work until S4.
- **`ModelWithEvents::events()` continues to emit `RunEvent::StreamChunk / StreamToolCall / StreamEnd`** as today. S1 only changes how `StreamDelta` is structured internally. For `StreamDelta::Error` (the new variant for Anthropic-protocol errors), `ModelWithEvents` emits **`RunEvent::Error { message }`** (the existing variant) in S1. Adding a structured `RunEvent::StreamError { code, message }` is deferred to S6 (config + WebUI + docs cleanup).
- **Status-classifier function `classify_status(u16) -> &'static str` lives in `src/model/anthropic.rs`** as `pub(crate)`. The string values are: `bad_request`, `auth`, `forbidden`, `not_found`, `context_overrun`, `rate_limit`, `server_error`, `overload`, `unknown`.
- **`AnthropicModel::complete()` and `complete_stream()` use `todo!()`** with a descriptive message. They satisfy the trait, but panic if called. S2 implements them.
- **The new `src/model/types.rs` and `src/model/anthropic.rs` files must compile cleanly** under the same toolchain as the rest of the crate (`rust-toolchain.toml` if present — otherwise whatever `rustup default` returns).
- **Per project memory:** commit and push to origin/main after every task. No batching across commits within a single task; one logical commit per task.

## File Structure

```
src/model/
├── mod.rs                ← MODIFY: re-exports new types, new Model trait, ModelWithEvents re-aligned
├── types.rs              ← NEW:   provider-agnostic types (ContentBlock, ModelRequest, ...)
├── anthropic.rs          ← NEW:   AnthropicConfig + AnthropicModel skeleton + classify_status()
├── openai_compat.rs      ← MODIFY: boundary adapter, no wire-protocol change
└── streaming.rs          ← MODIFY: generic param renamed to use new StreamDelta
```

Tests live in `#[cfg(test)] mod tests` blocks within each new file. No new top-level test files.

---

### Task 1: Pre-flight + baseline + read existing wiring

**Files:** Read-only. No edits.

**Prerequisites:** none.

- [ ] **Step 1: Verify crate name and toolchain**

```bash
cd /home/hhhh/Graph-Centric
grep '^name = ' Cargo.toml
ls rust-toolchain.toml 2>/dev/null || rustup show active-toolchain
```

Expected: prints `name = "graph_harness"` and a stable toolchain like `stable-x86_64-unknown-linux-gnu` (or no rust-toolchain.toml — both are fine).

- [ ] **Step 2: Baseline build + test pass**

```bash
cd /home/hhhh/Graph-Centric
cargo build --bin serve 2>&1 | tail -10
echo "---"
cargo test --lib 2>&1 | tail -5
```

Expected: build succeeds with `Finished \`dev\`` line; tests output ends with something like `test result: ok. N passed` (N is whatever the existing count is, per CLAUDE.md about 606).

- [ ] **Step 3: Read every file you will touch**

In parallel reads:
- `src/model/mod.rs` — find the current `Model` trait signature, the `ModelWithEvents` definition, and any `pub use` exports.
- `src/model/openai_compat.rs` — find the request-building function, the SSE stream parser, and the response-mapping function. Note what shape the OpenAI side expects (tool_calls array, choices[0].message) so the boundary adapter can be mechanical.
- `src/model/streaming.rs` — find how `RunEvent` is emitted from a stream (look for `StreamChunk`, `StreamToolCall`, `StreamEnd`).
- `src/events.rs` (if present, web layer) — sanity-check the `RunEvent` enum to confirm `StreamToolCall` variants.

- [ ] **Step 4: Sketch the boundary adapter shape**

Write down (on paper or in a comment in your scratch area — not in code) how the OpenAI shape's `choices[0].message.tool_calls[N]` maps to `Vec<ContentBlock>::ToolUse { id, name, input }` and how `choices[0].message.content: String` maps to `Vec<ContentBlock>::Text { text }`. Confirm with yourself that the inverse map (ContentBlock → OpenAI tool_call fragments) is also mechanical.

This pre-flight produces no commit — just validates the baseline and feeds you the context you need.

---

### Task 2: Add `src/model/types.rs` with provider-agnostic types + serde round-trip tests

**Files:**
- Create: `src/model/types.rs` (~150-200 lines including tests)

**Prerequisites:** Task 1 complete.

- [ ] **Step 1: Create the types module**

Write `src/model/types.rs` with this exact content:

```rust
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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentBlockDelta {
    TextDelta(String),
    ThinkingDelta(String),
    InputJsonDelta(String), // partial JSON; client concatenates then parses
}

// ---------- Messages ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Role {
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
```

- [ ] **Step 2: Add the test module at the bottom of `src/model/types.rs`**

Append this to the file:

```rust
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
```

- [ ] **Step 3: Verify the types module compiles via a one-off `rustc` smoke check**

The full crate build will see this file only after Task 3 declares `mod types;` in `mod.rs`. To verify the file parses cleanly in isolation right now, run:

```bash
cd /home/hhhh/Graph-Centric
# Standalone syntax/type sanity. --crate-type lib lets a .rs file with no
# mod-declaration-ancestor be type-checked as if it were a library root.
# Errors here are syntax/typing bugs in our type definitions.
rustc --edition 2024 --crate-type lib \
  --extern serde_json --extern serde \
  src/model/types.rs 2>&1 | tail -10 || true
```

Expected: a long stream of "undefined crate" errors is **fine and expected** here (we don't pull in this file's deps for this one-off check) — that's NOT a compilation failure of OUR code. What we're looking for is **no errors mentioning `src/model/types.rs:N:M`**. If `rustc` reports an error referencing a line in `types.rs`, that's a real type/syntax bug to fix.

If `rustc` is not in PATH, fall back to: run Task 3 first, then `cargo build --bin serve` (Task 3 Step 5 catches everything this step would have caught).

- [ ] **Step 4: Tests get exercised at the end of the plan, not per-task**

The unit tests in this file are gated by `#[cfg(test)]`. They compile-check during the full `cargo build` in Task 3 Step 5 and run during the full `cargo test --lib` in Task 5 Step 5. No per-task test run is needed; the cost of running tests after every small file edit is high and they will all run together at the end.

---

### Task 3: Update `src/model/mod.rs` — new trait + re-exports + `ModelWithEvents` re-align

**Files:**
- Modify: `src/model/mod.rs` (re-export types, redefine `Model`, re-align `ModelWithEvents` generic param)

**Prerequisites:** Task 2 complete.

- [ ] **Step 1: Read current `src/model/mod.rs`** (you did in Task 1; do it again if you need to)

Take a snapshot of the current content. The new file should:

1. Re-export `ContentBlock`, `ContentBlockInit`, `ContentBlockDelta`, `Role`, `ConversationMessage`, `ToolResult`, `ToolSchema`, `StopReason`, `Usage`, `ModelRequest`, `ModelResponse`, `StreamDelta` from `types`.
2. Add `mod types;` declaration.
3. Add `mod anthropic;` declaration (Task 4 creates the file).
4. Keep `mod openai_compat;` and `mod streaming;` declarations.
5. Redefine `Model` trait per spec (`complete`, `complete_stream`, `provider_name`).
6. Re-align `ModelWithEvents` to consume `StreamDelta` and emit `RunEvent` (existing shape — no changes to `RunEvent`).

- [ ] **Step 2: Rewrite the imports block**

Replace whatever top-of-file import block currently exists with:

```rust
//! `model` module — provider-agnostic Model trait + types.
//!
//! S1 of the OpenAI -> Anthropic migration introduces the in-memory types in
//! `types.rs`, keeps `openai_compat.rs` as a bridge adapter that translates
//! between OpenAI wire format and the new types, and adds a skeleton
//! `AnthropicModel` whose `complete`/`complete_stream` bodies are filled in
//! by S2.

pub mod anthropic;
pub mod openai_compat;
pub mod streaming;
pub mod types;

// Public re-exports — these names are imported by callers throughout `src/`.
pub use types::{
    ContentBlock, ContentBlockDelta, ContentBlockInit, ConversationMessage,
    ModelRequest, ModelResponse, Role, StopReason, StreamDelta, ToolResult,
    ToolSchema, Usage,
};

use anyhow::Result;
use async_trait::async_trait;
use futures_util::stream::BoxStream;
```

(The exact `use` ordering may need to match the crate's existing style — read `src/lib.rs` or whichever file currently imports from this module to confirm if anything else must come along for the ride.)

- [ ] **Step 3: Replace the existing `pub trait Model` block with the new shape**

```rust
/// Provider-agnostic interface to any LLM backend.
///
/// `openai_compat::OpenAiCompatModel` and `anthropic::AnthropicModel` both
/// implement this. `ModelWithEvents` wraps either and re-emits the stream
/// as `RunEvent`s.
#[async_trait]
pub trait Model: Send + Sync {
    /// Single-shot non-streaming call. Default implementation may aggregate
    /// a stream into one `ModelResponse`; primary impls override with a
    /// direct non-streaming endpoint.
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse>;

    /// SSE-style streaming call. Returns a stream of `StreamDelta`s that
    /// correspond to Anthropic's `content_block_*` events plus a terminal
    /// `MessageEnd`.
    async fn complete_stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<StreamDelta>>>;

    /// Short stable identifier for the provider, used in logging and
    /// per-run event tagging. Default `"anthropic"` since S1 is a hard migration.
    fn provider_name(&self) -> &'static str { "anthropic" }
}
```

This adds the new methods. The old `Model` trait (whatever shape it had before — likely just `complete()` returning some intermediate type) is replaced.

- [ ] **Step 4: Update `ModelWithEvents` to consume `StreamDelta`**

Open `src/model/streaming.rs`. Find the existing `ModelWithEvents` struct. Update its `events()` method body (or whichever method returns the `RunEvent` stream) so it pattern-matches on `StreamDelta` and emits the existing `RunEvent::StreamChunk` / `RunEvent::StreamToolCall` / `RunEvent::StreamEnd` variants:

```rust
// sketch — adapt to the actual streaming.rs structure you see in Task 1.

match delta {
    StreamDelta::ContentStart { index, init } => match init {
        ContentBlockInit::Text => {} // no-op, content arrives via Delta
        ContentBlockInit::Thinking => {} // ditto
        ContentBlockInit::ToolUse { id, name } => {
            // optional: emit a "tool_use_start" RunEvent here if the existing
            // events vocabulary has one; if not, no-op and wait for ContentDelta.
        }
    },
    StreamDelta::ContentDelta { index, delta } => match delta {
        ContentBlockDelta::TextDelta(t) => emit RunEvent::StreamChunk { ... },
        ContentBlockDelta::ThinkingDelta(t) => emit RunEvent::StreamChunk {
            chunk: StreamChunk::Reasoning(t),
        },
        ContentBlockDelta::InputJsonDelta(j) => {
            // accumulate by `index`; emit StreamToolCall once per tool_use
            // block when ContentStop arrives.
        }
    },
    StreamDelta::ContentStop { index } => {
        // if a tool_use block has accumulated, emit StreamToolCall here.
    }
    StreamDelta::MessageEnd { stop_reason, usage } => emit RunEvent::StreamEnd { ... },
    StreamDelta::Error { code, message } => emit RunEvent::StreamError { code, message },
}
```

The exact mapping of which `RunEvent` variant fires for which `StreamDelta` depends on the existing enum. Read `src/web/events.rs` (or wherever `RunEvent` lives) and adapt.

- [ ] **Step 5: Build verification — must succeed**

```bash
cd /home/hhhh/Graph-Centric
cargo build --bin serve 2>&1 | tail -8
```

Expected: build succeeds. If it fails, the most likely cause is `openai_compat.rs` still implementing the OLD `Model` trait — that's what Task 5 fixes. If you can't reach a green build here, **stop and ask before touching Task 5** — it suggests Task 3+4 changes have leaked somewhere they shouldn't.

- [ ] **Step 6: Don't commit yet**

Task 5 (openai_compat boundary) is the make-or-break follow-up. Run that first, then commit Tasks 2-3-4-5 together as one logical PR (since they cannot function independently).

---

### Task 4: Add `src/model/anthropic.rs` — `AnthropicConfig`, `AnthropicModel` skeleton, `classify_status`

**Files:**
- Create: `src/model/anthropic.rs`

**Prerequisites:** Task 3's `mod anthropic;` declaration makes this file loadable. Tasks 2+3 already added.

- [ ] **Step 1: Write `src/model/anthropic.rs`**

```rust
//! Anthropic protocol adapter — types and a skeleton `Model` impl.
//!
//! S1 introduces the configuration type, the skeleton struct, and the
//! `classify_status` helper. The actual HTTP client and SSE parser arrive
//! in S2. Until S2 lands, calling `complete` or `complete_stream` panics
//! with `todo!()`.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use super::types::{ModelRequest, ModelResponse, StreamDelta};
use super::Model;

const DEFAULT_BASE_URL: &str = "https://api.minimaxi.com/anthropic";
const DEFAULT_MODEL: &str = "MiniMax-M3";
const DEFAULT_MAX_RETRIES: u32 = 3;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Configuration for the Anthropic-protocol client.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_retries: u32,
    pub request_timeout: Duration,
}

impl AnthropicConfig {
    /// Defaults suitable for MiniMax's Anthropic-compatible endpoint.
    pub fn with_minimax_defaults(mut self) -> Self {
        if self.base_url.is_empty() { self.base_url = DEFAULT_BASE_URL.to_string(); }
        if self.model.is_empty() { self.model = DEFAULT_MODEL.to_string(); }
        self
    }
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: String::new(),
            model: DEFAULT_MODEL.to_string(),
            max_retries: DEFAULT_MAX_RETRIES,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

pub struct AnthropicModel {
    cfg: AnthropicConfig,
    http: reqwest::Client,
}

impl AnthropicModel {
    pub fn new(cfg: AnthropicConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(cfg.request_timeout)
            .build()
            .expect("reqwest client builder with timeout should not fail");
        Self { cfg, http }
    }

    pub fn config(&self) -> &AnthropicConfig { &self.cfg }

    /// Construct the standard Anthropic-protocol auth + version headers.
    /// MiniMax's anthropic-compat endpoint accepts `x-api-key`.
    pub fn auth_headers(&self) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&self.cfg.api_key) {
            h.insert(HeaderName::from_static("x-api-key"), v);
        }
        if let Ok(v) = HeaderValue::from_str(ANTHROPIC_VERSION) {
            h.insert(HeaderName::from_static("anthropic-version"), v);
        }
        h
    }
}

#[async_trait]
impl Model for AnthropicModel {
    async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse> {
        todo!("AnthropicModel::complete is implemented in S2")
    }

    async fn complete_stream(
        &self,
        _req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<StreamDelta>>> {
        todo!("AnthropicModel::complete_stream is implemented in S2")
    }

    fn provider_name(&self) -> &'static str { "anthropic" }
}

/// Map an HTTP status code to a stable short tag used in `anyhow::Context`
/// labels. The strings are part of the diagnostic contract — log consumers
/// depend on them being stable across releases, so don't change them lightly.
pub(crate) fn classify_status(status: u16) -> &'static str {
    match status {
        400 => "bad_request",
        401 => "auth",
        403 => "forbidden",
        404 => "not_found",
        413 => "context_overrun",
        429 => "rate_limit",
        500 => "server_error",
        529 => "overload",
        _   => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_status_known_codes() {
        assert_eq!(classify_status(400), "bad_request");
        assert_eq!(classify_status(401), "auth");
        assert_eq!(classify_status(403), "forbidden");
        assert_eq!(classify_status(404), "not_found");
        assert_eq!(classify_status(413), "context_overrun");
        assert_eq!(classify_status(429), "rate_limit");
        assert_eq!(classify_status(500), "server_error");
        assert_eq!(classify_status(529), "overload");
    }

    #[test]
    fn classify_status_unknown_codes() {
        assert_eq!(classify_status(200), "unknown");     // not an error code, but no panic
        assert_eq!(classify_status(418), "unknown");     // teapot, not in the table
        assert_eq!(classify_status(999), "unknown");     // arbitrary
    }

    #[test]
    fn auth_headers_contains_x_api_key_and_version() {
        let cfg = AnthropicConfig {
            api_key: "sk-test-xxxx".into(),
            ..Default::default()
        };
        let m = AnthropicModel::new(cfg);
        let h = m.auth_headers();
        assert_eq!(h.get("x-api-key").unwrap(), "sk-test-xxxx");
        assert_eq!(h.get("anthropic-version").unwrap(), "2023-06-01");
    }

    #[test]
    fn defaults_use_minimax_endpoint_and_m3() {
        let cfg = AnthropicConfig::default();
        assert_eq!(cfg.base_url, "https://api.minimaxi.com/anthropic");
        assert_eq!(cfg.model, "MiniMax-M3");
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.request_timeout, Duration::from_secs(60));
    }

    #[test]
    fn provider_name_is_anthropic() {
        let m = AnthropicModel::new(AnthropicConfig::default());
        assert_eq!(<AnthropicModel as Model>::provider_name(&m), "anthropic");
    }
}
```

- [ ] **Step 2: Build with `mod anthropic;` already declared**

```bash
cd /home/hhhh/Graph-Centric
cargo build --bin serve 2>&1 | tail -10
```

Expected: build succeeds. If it fails, check:
- `HeaderValue::from_str` not consuming the string — your call site may pass `&str` correctly (it does here).
- `async_trait` macro — the `#[async_trait]` on `impl Model for AnthropicModel` should be enough; `&self` becomes `&self`-shaped future.

- [ ] **Step 3: Run the new tests**

```bash
cd /home/hhhh/Graph-Centric
cargo test --lib model::anthropic 2>&1 | tail -10
```

Expected: all 5 anthropic tests pass.

---

### Task 5: Adapt `openai_compat.rs` to bridge OpenAI wire format ↔ new `Model` types

**Files:**
- Modify: `src/model/openai_compat.rs` (no wire-protocol change — only the in-memory types are reshaped)

**Prerequisites:** Tasks 2-4 complete; in particular, the new `Model` trait and types exist.

This is the **risk-heavy** task. The OpenAI HTTP wire format (`POST /v1/chat/completions` with `tools: [{type: "function", function: {name, description, parameters}}]` and SSE parsing of `tool_calls[N]` deltas) is preserved verbatim. But the in-memory struct types used by `OpenAiCompatModel` change from whatever-current-shape to the new `ModelRequest` / `ModelResponse` / `StreamDelta`. The file must keep the same externally-visible behavior or runtime will break.

- [ ] **Step 1: Inventory the existing types in `openai_compat.rs`**

Look for:
- Whatever current shape struct holds the request body sent to OpenAI (`ChatCompletionRequest` or similar).
- The response struct (`ChatCompletionResponse` with `choices: Vec<Choice>`).
- The streaming-chunk struct (`ChatCompletionChunk`).
- The function-call shape inside the response (`ToolCall` with `id: String`, `function: { name, arguments: String }`).
- The SSE parser that turns `data: {...}` lines into chunks.
- The function that collects chunks into a final response.

- [ ] **Step 2: Write a boundary adapter module-or-section**

The simplest pattern: keep the wire-format structs local to `openai_compat.rs` (under `mod wire` or just `pub(super) struct` with `Wire*` prefix), and add **FOUR** functions (not three — the implementer may have underestimated the scope):

```rust
// At the top of openai_compat.rs (or wherever fits the layout):

use crate::model::types::{
    ContentBlock, ContentBlockDelta, ContentBlockInit, ModelRequest,
    ModelResponse, Role, StopReason, StreamDelta, ToolResult, ToolSchema,
    Usage, ConversationMessage,
};

/// Translate a `ModelRequest` (new, Anthropic-shaped) into the OpenAI
/// wire-format body that goes in `POST /v1/chat/completions`.
fn req_to_openai_body(req: &ModelRequest) -> WireChatCompletionRequest { ... }

/// Translate an OpenAI wire-format response into a `ModelResponse`.
fn resp_from_openai(c: &WireChatCompletionResponse) -> ModelResponse { ... }

/// Translate a single OpenAI SSE chunk into zero-or-more `StreamDelta`s.
/// Holds a per-stream tool-state in `state: &mut ToolCallStreamState` for
/// per-index accumulation (see Task 1 report §4 for the state shape).
fn chunk_to_stream_deltas(c: &WireChatCompletionChunk, state: &mut ToolCallStreamState) -> Vec<StreamDelta> { ... }

/// FOURTH ADAPTER — for non-streaming callers: aggregate a `Vec<StreamDelta>`
/// produced by streaming into a single `ModelResponse`. The new `Model` trait
/// adds `complete_stream()` returning a `BoxStream`; some legacy
/// `complete()`-style callers (front-end "request a response, get one
/// ModelResponse" paths) want this for completeness. Optionally used by
/// `OpenAiCompatModel::complete` if you choose to give it a default
/// implementation that aggregates its own stream.
fn stream_deltas_to_response(deltas: Vec<StreamDelta>) -> ModelResponse { ... }
```

This is roughly a 150-250 line file if the existing adapter is mid-sized. Don't refactor more than necessary — the goal is "wire stays the same, types change."

**OpenAI wire-side struct changes required:**
- `WireMessage` (or whatever the local name is) needs a `tool_call_id: Option<String>` field. The current OpenAI tool-result shape is `Message { role: "tool", content, tool_call_id }` — your wire type currently has only `content`. Add the field; the boundary adapter fills it from `ConversationMessage::tool_results[i].tool_use_id`.

- [ ] **Step 3: Wire the new functions into `OpenAiCompatModel`**

`OpenAiCompatModel::complete(req: ModelRequest)` should:
1. Call `req_to_openai_body(&req)`.
2. POST and parse response with the wire structs.
3. Map back via `resp_from_openai`.
4. Return `ModelResponse`.

`OpenAiCompatModel::complete_stream(req: ModelRequest)` should:
1. Call `req_to_openai_body`.
2. Open the SSE stream.
3. For each chunk, call `chunk_to_stream_deltas` and forward them downstream (boxed).
4. After the SSE stream ends, emit a synthetic `StreamDelta::MessageEnd { stop_reason, usage }` based on whatever the final chunk said (or `StopReason::Unknown` if undetermined).

- [ ] **Step 4: Build verification**

```bash
cd /home/hhhh/Graph-Centric
cargo build --bin serve 2>&1 | tail -15
```

Expected: build succeeds. **If it doesn't**, the most common failure is type mismatch on the `messages` field — `WireChatCompletionRequest::messages` was something like `Vec<WireMessage>` previously; you're now feeding it `Vec<ConversationMessage>` from `req_to_openai_body`, so you must map to the wire type before passing it on.

A second likely failure: `OpenAiCompatModel` still implements the OLD `Model` trait (whatever pre-S1 shape that was). The `impl Model for OpenAiCompatModel` block must match the new trait from Task 3.

If anything gets gnarly, **don't fight it** — revert your changes and ask the controller for guidance.

- [ ] **Step 5: Run the full test suite — confirm zero regression**

```bash
cd /home/hhhh/Graph-Centric
cargo test --lib 2>&1 | tail -15
```

Expected: **652 + 12 new (7 types + 5 anthropic) = 664 passing**, with the same 1 pre-existing failure (`validator_passed_lets_loop_proceed_to_review`, unrelated to model layer) present. Track delta only, not absolute. If a previously-passing test flipped to failing → regression; if a previously-failing unrelated test now passes → bonus, not a requirement.

- [ ] **Step 6: One final smoke build**

```bash
cd /home/hhhh/Graph-Centric
cargo build --release --bin serve 2>&1 | tail -5
```

Expected: release build succeeds. Just confirms there are no debug-only compilation paths that we missed.

---

### Task 6: Commit + push

**Files:** all the touched files from Tasks 2-5.

**Prerequisites:** Tasks 1-5 complete; the build is green.

- [ ] **Step 1: Stage and commit**

```bash
cd /home/hhhh/Graph-Centric
git status --short
git add src/model/types.rs src/model/anthropic.rs src/model/mod.rs src/model/openai_compat.rs src/model/streaming.rs
git diff --staged --stat
git commit -m "refactor(model): introduce provider-agnostic types for OpenAI->Anthropic migration S1

S1 of 6 sub-projects. Adds:
- src/model/types.rs with ContentBlock { Text, Thinking, ToolUse },
  ModelRequest (top-level system field), ModelResponse.blocks,
  Anthropic-native StreamDelta events, plus 7 serde round-trip tests.
- src/model/anthropic.rs with AnthropicConfig (default MiniMax endpoint
  + M3 model), AnthropicModel struct + auth_headers(), status classifier,
  and 5 unit tests. complete()/complete_stream() body left as todo!() for
  S2.
- src/model/mod.rs: Model trait gains complete_stream + provider_name;
  ModelWithEvents re-aligned to consume StreamDelta.
- src/model/openai_compat.rs: boundary adapter that translates OpenAI
  wire format <-> new ModelRequest/Response/StreamDelta. Wire protocol
  unchanged. Caller layers (proposer, decomposer, enricher, verifier,
  reviewer, cascade, subagent) compile untouched.

S2 implements the HTTP client and SSE parser for AnthropicModel.
S4 migrates the 7 caller layers off openai_compat.
S5 wires MiniMax-M3 reasoning into ContentBlock::Thinking.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

- [ ] **Step 2: Push**

```bash
cd /home/hhhh/Graph-Centric
git push origin main 2>&1 | tail -3
```

Network may hiccup; if "Connection closed", `sleep 2-5` and retry.

- [ ] **Step 3: Verify clean working tree on both local and remote**

```bash
cd /home/hhhh/Graph-Centric
git log --oneline -1
git status --short
git log origin/main --oneline -1
# verify local HEAD == origin/main HEAD
```

Expected: identical commit on local and origin/main; working tree clean.

- [ ] **Step 4: Write the implementation report**

Save to `/home/hhhh/Graph-Centric/.superpowers/sdd/s1-report.md` with:

- Brief summary (3-5 sentences) of what landed.
- The 7 + 5 = 12 new unit tests passing.
- `cargo build --bin serve` succeeds.
- `cargo test --lib` shows the same total pass count (or +12 for new tests).
- 1 commit on main.
- Any deviations from this plan.
- Self-review findings (what you noticed in retrospect).
- Anything S2 will need to know.

---

## Self-Review Checklist (run after Task 6 completes)

- [ ] `src/model/types.rs` exists, defines all 13 types listed in the spec, and has 7 tests inside `#[cfg(test)] mod tests`.
- [ ] `src/model/anthropic.rs` exists, defines `AnthropicConfig` and `AnthropicModel` with `todo!()` bodies for `complete` / `complete_stream`, and has 5 tests.
- [ ] `src/model/mod.rs` re-exports all 12 type names; `Model` trait has `complete`, `complete_stream`, `provider_name`.
- [ ] `src/model/openai_compat.rs` is adapted — its `Model` impl matches the new trait signatures; the wire format is unchanged from pre-S1.
- [ ] `src/model/streaming.rs` consumes `StreamDelta` and emits the same `RunEvent` shape as before (no `RunEvent` enum changes leak out).
- [ ] `cargo build --bin serve` green.
- [ ] `cargo test --lib` shows **652 baseline passing** + **12 new tests passing** = **664+** total, with the 1 pre-existing flake still present (unrelated).
- [ ] `cargo build --release --bin serve` green.
- [ ] Single git commit on main, pushed to origin/main, working tree clean.
- [ ] Report saved at `/home/hhhh/Graph-Centric/.superpowers/sdd/s1-report.md`.

## Out-of-Scope Reminder (deferred to later sub-projects)

- **S2**: `AnthropicModel::complete` and `complete_stream` HTTP implementations, SSE parser, retry-with-backoff for 429/529.
- **S3**: OpenAI compat drop. After S4 finishes and the 7 callers migrate, the `openai_compat.rs` file is deleted.
- **S4**: the 7 caller layers (`proposer.rs`, `decomposer.rs`, `enricher.rs`, `verifier.rs`, `reviewer.rs`, `cascade.rs`, `subagent.rs`) migrate to use `AnthropicModel` directly and shape their prompts around the new `ModelRequest.system` field.
- **S5**: `ContentBlock::Thinking` reasoning wiring for MiniMax-M3; `ModelRequest.extra` carries the `thinking` config; a new stream sub-channel in `openai_compat`/S2 emits thinking deltas.
- **S6**: webui Settings config (base URL, model name, advisor model), `CLAUDE.md` "Tool calls migration" + "Narrow protocols" retargeting to the Anthropic path.
