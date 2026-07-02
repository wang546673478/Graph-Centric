//! WebSocket handler: /ws/runs/:id
//!
//! Replaces the SSE event stream with a bidirectional WebSocket channel.
//! Each connected client gets events forwarded from the RunSession's
//! broadcast channel, and can send control messages (resume, repair,
//! set_detail_mode) back to the driver.

use super::events::RunEvent;
use super::run_session::RunSession;
use super::{RunId, WebState};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

pub async fn ws_handler(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state, id))
}

async fn handle_ws(socket: WebSocket, state: Arc<WebState>, id: RunId) {
    let session = {
        let runs = state.runs.read().await;
        match runs.get(&id) {
            Some(s) => s.clone(),
            None => {
                warn!(run_id = %id, "ws: run not found");
                return;
            }
        }
    };

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let mut detail_mode = false;

    // Subscribe to run events.
    let mut event_rx = session.event_tx.subscribe();

    // Spawn the event-forwarding half.
    let forward_session = session.clone();
    let mut forward_handle = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    // v2 spec §4.7: stamp with a per-run monotonic
                    // id so the frontend can detect missed
                    // events on reconnect.
                    let event_id = forward_session.next_event_id();
                    let ws_msg = run_event_to_ws_msg(&event, detail_mode, event_id);
                    if let Some(msg) = ws_msg {
                        if ws_sender.send(msg).await.is_err() {
                            break;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "ws: client lagging, events dropped");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        drop(forward_session);
    });

    // Process incoming client messages.
    while let Some(Ok(msg)) = ws_receiver.next().await {
        match msg {
            Message::Text(text) => {
                let parsed: Result<serde_json::Value, _> = serde_json::from_str(&text);
                if let Ok(json) = parsed {
                    let msg_type = json["type"].as_str().unwrap_or("");
                    match msg_type {
                        "resume" => {
                            if let Some(answer) = json["answer"].as_str() {
                                session
                                    .provide_answer(answer.to_string())
                                    .await;
                            }
                        }
                        "repair" => {
                            if let Ok(graph) = serde_json::from_value(json["graph"].clone()) {
                                session.provide_repair(graph).await;
                            }
                        }
                        "set_detail_mode" => {
                            detail_mode = json["enabled"].as_bool().unwrap_or(false);
                            debug!(detail_mode, "ws: detail mode toggled");
                        }
                        _ => {
                            debug!(msg_type, "ws: unknown client message type");
                        }
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    forward_handle.abort();
    info!(run_id = %id, "ws: client disconnected");
}

/// Convert a RunEvent to a WebSocket text message. Returns None for
/// events that should be filtered when detail_mode is off.
///
/// v2 spec §4.7: stamps every message with a monotonic `id` so
/// the frontend can detect missed events on reconnect. The
/// counter is per-run and lives on the session.
fn run_event_to_ws_msg(event: &RunEvent, detail_mode: bool, event_id: u64) -> Option<Message> {
    // Filter verbose events when detail mode is off.
    if !detail_mode {
        if let RunEvent::ModelCall { .. } | RunEvent::CascadeStep { .. } = event {
            return None;
        }
    }
    // Wrap the event JSON in a {"id": N, "data": {...}} envelope.
    let mut obj = serde_json::to_value(event).ok()?;
    if let serde_json::Value::Object(ref mut map) = obj {
        map.insert("id".to_string(), serde_json::json!(event_id));
    }
    let json = serde_json::to_string(&obj).ok()?;
    Some(Message::Text(json.into()))
}
