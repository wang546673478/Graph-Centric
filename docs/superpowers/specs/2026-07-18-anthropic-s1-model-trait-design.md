# OpenAI → Anthropic Migration S1 — Model Trait Abstraction

**Date:** 2026-07-18
**Status:** Implemented (with Option A scope amendment — see below)
**Owner:** user + assistant
**Series:** First of 6 sub-projects in the OpenAI → Anthropic migration.

```
S1 ← this spec
S2 → Anthropic SSE client
S3 → tool_calls protocol bridge
S4 → migrate 7 model layers to the new client
S5 → Reasoning/Thinking adaptation (M3)
S6 → Config / WebUI / docs cleanup
```

## Goal

Replace the wire-protocol-implicit types in `src/model/` with provider-agnostic types that
faithfully represent Anthropic's content-block model. Establish the `Model` trait shape
that the Anthropic-specific client (S2) will implement against.

This is **foundation work**: nothing user-facing changes yet. After S1 lands, the existing
`openai_compat.rs` continues to work (it's the only `Model` impl), but new code can be
written against the abstracted types.

## Scope

In scope:

- New file `src/model/types.rs` with `ContentBlock`, `ModelRequest`, `ModelResponse`,
  `StopReason`, `Usage`, `ConversationMessage`, `ToolSchema`, `ContentBlockInit`,
  `ContentBlockDelta`, `StreamDelta`.
- New file (or extension) `src/model/mod.rs` re-defines `Model` trait with:
  `complete()`, `complete_stream()`, `provider_name()`.
- New struct `AnthropicConfig` and `AnthropicModel` shell (constructor only; `complete()`
  implementation comes in S2, but the type skeleton is needed now so other code can refer
  to it without forward-declaration hacks).
- New `ModelWithEvents` wrapper signatures re-aligned to the new `StreamDelta` shape.
- `parse_*` helpers in call sites **untouched** in this S1 — S4 migrates them.
- Unit tests for type serde round-trips, error-context-tag assertions.

Out of scope (deferred to later sub-projects):

- **S2**: actual HTTP client wiring, SSE parser for Anthropic events. `complete()` and
  `complete_stream()` of `AnthropicModel` are stubbed in S1 with `unimplemented!()` or
  `todo!()` and panic with a clear message — that's acceptable in this scaffolding.
- **S3**: OpenAI ↔ Anthropic tool-call format conversion; this is moot once OpenAI is gone.
- **S4**: migrating the 7 caller layers (proposer, decomposer, enricher, verifier,
  reviewer, cascade, subagent) to the new client.
- **S5**: reasoning block extraction for MiniMax-M3 — Anthropic's `thinking` block
  decoding. `ContentBlock::Thinking` exists in S1, the parser lands in S5.
- **S6**: config + webui + CLAUDE.md rewrites.

## Pre-Flight Resolved

Before brainstorming started, three critical decisions were made (recorded per `using-
superpowers` rules so the design can reference them without re-asking):

| Decision | Choice |
|---|---|
| Provider | **MiniMax Anthropic-compatible endpoint** at `https://api.minimaxi.com/anthropic` |
| Default model | **MiniMax-M3** (reasoning model) |
| Auth | **x-api-key header** (MiniMax API key value) |
| Forward compat | Hard migration; no OpenAI impl kept after S4. Trait is provider-agnostic but only Anthropic impls exist. |
| Strategy | 6 incremental sub-projects, each with full brainstorm → spec → plan → impl cycle |

## Architecture

```
                 ┌──────────────────────────────────┐
                 │ src/model/types.rs (NEW)         │
                 │                                  │
                 │  ContentBlock                    │
                 │   ├─ Text                        │
                 │   ├─ Thinking       (S1 stub)   │  ← S5 wires M3 reasoning here
                 │   └─ ToolUse                     │
                 │  ModelRequest { system, ... }    │
                 │  ModelResponse { blocks, ... }   │
                 │  StreamDelta { Start/Δ/Stop/... } │
                 └──────────────────────────────────┘
                              ▲ ▲ ▲
                              │ │ │
                 ┌────────────┘ │ └────────────┐
                 │              │              │
        ┌────────┴─────┐  ┌──────┴───────┐  ┌────┴────────────┐
        │ openai_compat│  │ AnthropicModel│  │ ModelWithEvents │
        │   (.rs)      │  │  (stub S1)    │  │   (.rs)         │
        │  still wired │  │  complete=todo│  │ stream wrapper  │
        │  in S1       │  │  stream=todo  │  │ (re-aligned)    │
        └──────────────┘  └───────────────┘  └─────────────────┘
```

Two existing impls stay through S1-S3:
- `openai_compat.rs` — adapter from OpenAI shape to the new types. Continues to function
  through S3, removed in S4 once all 7 callers migrate.
- `ModelWithEvents` — wrapper that re-emits `StreamDelta` chunks as `RunEvent`s. Re-
  aligned so its generic parameter is `dyn Model` with the new trait.

## Type Definitions (sketch)

```rust
// src/model/types.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Thinking { thinking: String },                          // S5 wires M3 reasoning
    ToolUse { id: String, name: String, input: serde_json::Value },
}

#[derive(Debug, Clone, Default)]
pub struct ModelRequest {
    pub system: Option<String>,                              // Anthropic top-level field
    pub messages: Vec<ConversationMessage>,
    pub tools: Vec<ToolSchema>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop: Vec<String>,
    pub extra: HashMap<String, serde_json::Value>,           // extension hook
}

#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub id: String,
    pub model: String,
    pub blocks: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy)]
pub enum StopReason { EndTurn, MaxTokens, ToolUse, StopSequence, Unknown }

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_input_tokens: Option<u32>,
    pub cache_creation_input_tokens: Option<u32>,
}

// Streaming events — Anthropic-native shape.

#[derive(Debug, Clone)]
pub enum StreamDelta {
    ContentStart { index: u32, init: ContentBlockInit },     // content_block_start
    ContentDelta { index: u32, delta: ContentBlockDelta },   // content_block_delta
    ContentStop  { index: u32 },                              // content_block_stop
    MessageEnd   { stop_reason: StopReason, usage: Usage },  // message_delta + message_stop
    Error        { code: Option<u32>, message: String },     // Anthropic event: error
}

#[derive(Debug, Clone)]
pub enum ContentBlockInit {
    Text,
    Thinking,
    ToolUse { id: String, name: String },
}

#[derive(Debug, Clone)]
pub enum ContentBlockDelta {
    TextDelta(String),
    ThinkingDelta(String),
    InputJsonDelta(String),    // partial JSON, client concatenates then parses
}

#[derive(Debug, Clone)]
pub struct ConversationMessage {
    pub role: Role,                  // User / Assistant
    pub blocks: Vec<ContentBlock>,   // assistant can produce tool_use; user can produce tool_result
}

#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,   // JSON Schema (Anthropic format; OpenAI's "parameters" maps here)
}
```

## Trait Shape

```rust
// src/model/mod.rs

#[async_trait]
pub trait Model: Send + Sync {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse>;

    async fn complete_stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<StreamDelta>>>;

    fn provider_name(&self) -> &'static str { "anthropic" }
}

// AnthropicModel — S2 implements complete()/complete_stream(). S1 only adds the type skeleton.

pub struct AnthropicConfig {
    pub base_url: String,           // default "https://api.minimaxi.com/anthropic"
    pub api_key: String,
    pub model: String,             // default "MiniMax-M3"
    pub max_retries: u32,          // default 3
    pub request_timeout: Duration, // default 60s
}

pub struct AnthropicModel {
    cfg: AnthropicConfig,
    http: reqwest::Client,
}

impl AnthropicModel {
    pub fn new(cfg: AnthropicConfig) -> Self { /* construct reqwest::Client */ }

    fn auth_headers(&self) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", self.cfg.api_key.parse().unwrap());
        h.insert("anthropic-version", "2023-06-01".parse().unwrap());
        h
    }
}

// concrete Model impl lands in S2.

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
```

## Error Handling

Per the resolved scope decision: `anyhow::Error` with `Context` tags so callers can introspect.

```rust
// Pattern used in S2 — previewed here so S1's tests can assert the Context-tag behavior:

use anyhow::{anyhow, Context, Result};

fn classify_status(status: u16) -> &'static str {
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
```

The 429/529 retry-with-backoff happens inside `AnthropicModel::complete_stream` (S2).
Other statuses are surfaced as errors with the relevant Context tag immediately.

## Files

### Created

| Path | Purpose |
|---|---|
| `src/model/types.rs` | All provider-agnostic types listed above. |
| `src/model/anthropic.rs` | `AnthropicConfig`, `AnthropicModel`, and the stub `Model` impl. |

### Modified

| Path | Change |
|---|---|
| `src/model/mod.rs` | Re-export new types. `Model` trait gains `complete_stream` and `provider_name`. `ModelWithEvents` re-aligned to `StreamDelta`. |
| `src/model/openai_compat.rs` | Continues to work; **no edits in S1**. The OpenAI `chat/completions` API still produces `Complete`/`Stream` types; we'll re-target in S3. S1 leaves the file alone. |

### Untouched

- All 7 caller layers (`proposer.rs`, `decomposer.rs`, `enricher.rs`, `verifier.rs`,
  `reviewer.rs`, `cascade.rs`, `subagent.rs`) — untouched in S1.
- `src/model/streaming.rs` — same shape, only rename type params.
- Anything outside `src/model/**`.

## Tests

This sub-project introduces the test scaffolding too (project has none yet for model
layer — verified).

```rust
// src/model/types.rs (test module)

#[test]
fn content_block_serde_round_trip_text() {
    let block = ContentBlock::Text { text: "hello".into() };
    let json = serde_json::to_string(&block).unwrap();
    let back: ContentBlock = serde_json::from_str(&json).unwrap();
    assert_eq!(format!("{:?}", back), format!("{:?}", block));
}

#[test]
fn content_block_serde_round_trip_tool_use() {
    let block = ContentBlock::ToolUse {
        id: "toolu_abc".into(),
        name: "propose_patch".into(),
        input: serde_json::json!({"patches": []}),
    };
    let json = serde_json::to_string(&block).unwrap();
    let back: ContentBlock = serde_json::from_str(&json).unwrap();
    assert_eq!(format!("{:?}", back), format!("{:?}", block));
}

#[test]
fn stream_delta_serde_round_trip_content_start() {
    let delta = StreamDelta::ContentStart {
        index: 0,
        init: ContentBlockInit::Text,
    };
    let json = serde_json::to_string(&delta).unwrap();
    let back: StreamDelta = serde_json::from_str(&json).unwrap();
    assert_eq!(format!("{:?}", back), format!("{:?}", delta));
}
```

Plus a test of the Context-tag classification:

```rust
#[test]
fn status_classification_tags_have_expected_names() {
    assert_eq!(classify_status(401), "auth");
    assert_eq!(classify_status(429), "rate_limit");
    assert_eq!(classify_status(529), "overload");
    assert_eq!(classify_status(400), "bad_request");
    assert_eq!(classify_status(999), "unknown");
}
```

Run with `cargo test -p graph-centric model::`.

## Migration Risk

Low. S1 introduces types but does **not yet rewire anything**. Existing
`openai_compat.rs` continues to compile against the new `ModelRequest`/`ModelResponse`
types as long as I update its serialization at the call boundary (one PR-sized change
in S1's commit body). If that update is too risky, defer the openai_compat adaptation to
S3 — but every layer compiles must keep that file compiling in S1.

## Exit Criteria

1. `cargo build --bin serve` succeeds with `src/model/types.rs` and `src/model/anthropic.rs`
   added; `src/model/mod.rs` updated.
2. `cargo test -p graph-centric model::` runs the new unit tests; all pass.
3. `openai_compat.rs` and `ModelWithEvents` still compile against the new types (with the
   boundary adapter I commit in the same task).
4. The 7 caller layers compile unchanged — no `cargo build` warning about unused
   `ContentBlock::*` variants yet (those land in S4).
5. Commit pushed to `origin/main`.

## Out of Scope Reminder (post-Option-A)

This spec ends with `src/model/types.rs` (Anthropic-native types), declared
`pub(crate)` in `src/model/mod.rs`. Existing public `Message` / `Role` / `Model`
surface is preserved unchanged. **No HTTP client code yet** (S2 — re-scoped
under Option A). **No caller changes yet** (S4). **No M3-reasoning adapter
yet** (S5).

## Status — Implementation Outcome (Option A)

The original S1 plan called for `pub use types::{...}` re-exports, a new `Model`
trait with `BoxStream<Result<StreamDelta>>` signatures, and an `openai_compat.rs`
boundary adapter. Task 3 implementation surfaced a critical issue: the project
has **30+ import sites** under `agent/*`, `web/*`, `skills/*`, `bin/*` that
reference the old `crate::model::{Message, Role, ToolCall, FinishReason, Usage,
StreamDelta}` shape directly. Replacing `mod.rs` re-exports with new types
broke all of them at once.

**Resolution (Option A, user-chosen 2026-07-18):** introduce the new types
module as a **private, crate-internal** sibling to the existing public type
surface. AnthropicModel (S2) and the caller-migration work (S4) will use the
new types; existing callers continue using the legacy types until S4 rewires
them. This preserves the CLAUDE.md principle of "Narrow protocols at the
boundary" — the model layer is a boundary, and two parallel type systems
on either side of it are kept narrow.

**What landed in S1 (`39774c0`):**
- `src/model/types.rs` with 7 round-trip tests, declared `pub(crate) mod types;`.
- `src/model/mod.rs` adds the bare `pub(crate) mod types;` declaration with a
  comment block referencing S2/S4/S5/S6.
- Build green, 659 passing + 1 pre-existing flake (no caller regressions;
  the existing `validator_passed_lets_loop_proceed_to_review` flake remained).

**Deferred (re-scoped):**
- AnthropicModel skeleton + `classify_status` helper → **moves to S2.**
- `openai_compat.rs` boundary adapter → **moves to S4** (caller migration).
- The new `Model` trait shape (`BoxStream<Result<StreamDelta>>`) and the
  translation at the AnthropicModel boundary → **designed during S2
  brainstorming, not before**.

See `docs/superpowers/plans/2026-07-18-anthropic-s1-model-trait.md` for
the updated task list and rationale.
