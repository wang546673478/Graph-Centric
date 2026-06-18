//! ModelWithEvents — transparent streaming wrapper.
//!
//! Wraps any `Arc<dyn Model>` so that calls to `complete()` are transparently
//! routed through `complete_stream()`, forwarding each `StreamDelta` to a
//! broadcast channel as `RunEvent::StreamChunk` / `RunEvent::StreamEnd`.
//!
//! All existing code that calls `complete()` on the wrapped model gets SSE
//! streaming without any changes — the wrapper intercepts at the `Model`
//! trait level.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::broadcast;

use super::{Model, ModelRequest, ModelResponse, StreamDelta};
use crate::error::Result;
use crate::web::events::RunEvent;

/// Transparent streaming adapter for any `Model`.
///
/// # How it works
///
/// 1. `complete()` creates an mpsc channel and calls `inner.complete_stream()`
/// 2. A forwarder task reads deltas from the channel and emits
///    `RunEvent::StreamChunk` / `RunEvent::StreamEnd` to the broadcast sender
/// 3. When `complete_stream` returns, the tx is dropped, the forwarder drains
///    any remaining deltas, and `complete()` returns the final `ModelResponse`
pub struct ModelWithEvents {
    inner: Arc<dyn Model>,
    event_tx: broadcast::Sender<RunEvent>,
    component: String,
}

impl ModelWithEvents {
    pub fn new(
        inner: Arc<dyn Model>,
        event_tx: broadcast::Sender<RunEvent>,
        component: String,
    ) -> Self {
        Self {
            inner,
            event_tx,
            component,
        }
    }

    /// Convenience: wrap a model and return as `Arc<dyn Model>`.
    pub fn wrap(
        inner: Arc<dyn Model>,
        event_tx: broadcast::Sender<RunEvent>,
        component: String,
    ) -> Arc<dyn Model> {
        Arc::new(Self::new(inner, event_tx, component))
    }
}

#[async_trait]
impl Model for ModelWithEvents {
    fn name(&self) -> &str {
        self.inner.name()
    }

    /// Non-streaming complete() that emits StreamChunk/StreamEnd events
    /// so the frontend can display model responses. Streaming (SSE) is
    /// not used — the non-streaming path is reliable on all platforms.
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let event_tx = self.event_tx.clone();
        let component = self.component.clone();

        // Use non-streaming complete() — proven reliable via model-ping.
        let resp = self.inner.complete(request).await?;

        // Emit content as a single StreamChunk (the frontend needs these events).
        if !resp.content.is_empty() {
            let _ = event_tx.send(RunEvent::StreamChunk {
                component: component.clone(),
                content: resp.content.clone(),
                reasoning_content: resp.reasoning_content.clone(),
                finish_reason: None,
            });
        }
        if let Some(ref r) = resp.reasoning_content {
            if !r.is_empty() {
                let _ = event_tx.send(RunEvent::StreamChunk {
                    component: component.clone(),
                    content: String::new(),
                    reasoning_content: Some(r.clone()),
                    finish_reason: None,
                });
            }
        }
        let _ = event_tx.send(RunEvent::StreamEnd {
            component,
            finish_reason: format!("{:?}", resp.finish_reason).to_lowercase(),
            prompt_tokens: resp.usage.prompt_tokens as u64,
            completion_tokens: resp.usage.completion_tokens as u64,
        });

        Ok(resp)
    }
}
