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

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let event_tx = self.event_tx.clone();
        let component = self.component.clone();

        // Forward deltas to the WebSocket broadcast channel.
        let fwd = tokio::spawn(async move {
            while let Some(delta) = rx.recv().await {
                match delta {
                    StreamDelta::Delta {
                        content,
                        reasoning_content,
                    } => {
                        let _ = event_tx.send(RunEvent::StreamChunk {
                            component: component.clone(),
                            content,
                            reasoning_content,
                            finish_reason: None,
                        });
                    }
                    StreamDelta::Done {
                        finish_reason,
                        usage,
                    } => {
                        let _ = event_tx.send(RunEvent::StreamEnd {
                            component: component.clone(),
                            finish_reason: format!("{:?}", finish_reason).to_lowercase(),
                            prompt_tokens: usage.prompt_tokens as u64,
                            completion_tokens: usage.completion_tokens as u64,
                        });
                    }
                }
            }
        });

        // Drive the inner model's streaming call.
        let result = self.inner.complete_stream(request, tx).await;

        // Wait for the forwarder to finish draining. The tx was dropped
        // when complete_stream returned, so rx will yield None shortly.
        let _ = fwd.await;

        result
    }
}
