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

    /// Stream-first with non-streaming fallback (Claude Code pattern).
    /// 1. Try SSE streaming with a 90s idle watchdog.
    /// 2. If streaming hangs (no delta within 90s), fall back to
    ///    non-streaming `inner.complete()`.
    /// 3. If streaming produces data, use it.
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let event_tx = self.event_tx.clone();
        let component = self.component.clone();

        // Forward deltas to the WebSocket broadcast channel.
        let fwd = tokio::spawn(async move {
            while let Some(delta) = rx.recv().await {
                match delta {
                    StreamDelta::Delta { content, reasoning_content } => {
                        let _ = event_tx.send(RunEvent::StreamChunk {
                            component: component.clone(),
                            content,
                            reasoning_content,
                            finish_reason: None,
                        });
                    }
                    StreamDelta::Done { finish_reason, usage } => {
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

        // Clone request for potential non-streaming fallback.
        let fallback_req = ModelRequest {
            messages: request.messages.clone(),
            tools: request.tools.clone(),
            temperature: request.temperature,
            max_tokens: request.max_tokens,
            stop: request.stop.clone(),
        };

        // Stream with 90s timeout (Claude Code: CLAUDE_STREAM_IDLE_TIMEOUT_MS).
        // tokio::select! ensures the timeout always fires regardless of
        // whether the stream future is cancellable.
        let stream_fut = self.inner.complete_stream(request, tx);
        tokio::pin!(stream_fut);
        let sleep = tokio::time::sleep(std::time::Duration::from_secs(90));
        tokio::pin!(sleep);

        let stream_result = tokio::select! {
            res = &mut stream_fut => Some(res),
            _ = &mut sleep => None,
        };

        match stream_result {
            Some(Ok(resp)) => {
                let _ = fwd.await;
                Ok(resp)
            }
            _ => {
                tracing::warn!(
                    component = %self.component,
                    "streaming timed out or failed; falling back to non-streaming"
                );
                let result = self.inner.complete(fallback_req).await;
                let _ = fwd.await;
                result
            }
        }
    }
}
