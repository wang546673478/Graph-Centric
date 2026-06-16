//! Per-run state machine. One `RunSession` per active or completed run.

use super::checkpoint::CheckpointStore;
use super::events::{EdgeDto, NodeDto, RunEvent};
use crate::agent::reviewer::ReviewResult;
use crate::graph::Graph;
use crate::skills::SkillRef;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, Notify};
use tokio_util::sync::CancellationToken;

const EVENT_CHANNEL_CAPACITY: usize = 256;

/// One run's state. The `RunSession` is the single source of truth for
/// a run; the SSE handler subscribes to `event_tx` and forwards events
/// to the browser.
pub struct RunSession {
    pub id: String,
    pub task: String,
    pub started_at: Instant,
    pub status: tokio::sync::RwLock<RunStatus>,
    pub event_tx: broadcast::Sender<RunEvent>,
    pub cancel: CancellationToken,
    /// Resolved by `POST /api/runs/{id}/answer` when the run is `Paused`.
    pub pending_answer: Notify,
    pub pending_answer_value: tokio::sync::Mutex<Option<String>>,
    /// Resolved by `POST /api/runs/{id}/repair` when the run is `GraphInvalid`.
    pub pending_repair: Notify,
    pub pending_repair_value: tokio::sync::Mutex<Option<Graph>>,
    /// Last known graph (kept in sync with broadcast events).
    pub last_graph: tokio::sync::RwLock<Arc<Graph>>,
    /// Last known review result (if any).
    pub last_review: tokio::sync::RwLock<Option<ReviewResult>>,
    /// The captured skill (if the run completed with Pass and capture succeeded).
    pub captured_skill: tokio::sync::RwLock<Option<SkillRef>>,
    /// v2: per-run checkpoint store for branching and replay.
    pub checkpoints: tokio::sync::Mutex<CheckpointStore>,
}

/// Run state visible to the API and the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Paused,
    GraphInvalid,
    Done,
    Error(String),
    Cancelled,
}

impl RunStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Cancelled | Self::Error(_))
    }
}

impl RunSession {
    pub fn new(id: String, task: String) -> Self {
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            id,
            task,
            started_at: Instant::now(),
            status: tokio::sync::RwLock::new(RunStatus::Running),
            event_tx,
            cancel: CancellationToken::new(),
            pending_answer: Notify::new(),
            pending_answer_value: tokio::sync::Mutex::new(None),
            pending_repair: Notify::new(),
            pending_repair_value: tokio::sync::Mutex::new(None),
            last_graph: tokio::sync::RwLock::new(Arc::new(Graph::new())),
            last_review: tokio::sync::RwLock::new(None),
            captured_skill: tokio::sync::RwLock::new(None),
            checkpoints: tokio::sync::Mutex::new(CheckpointStore::new()),
        }
    }

    /// Broadcast an event to all SSE subscribers. No-op if there are no
    /// active subscribers (the channel is bounded and will drop lagged
    /// subscribers gracefully).
    pub fn emit(&self, event: RunEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Snapshot the current graph as a `RunEvent::GraphSnapshot` and
    /// emit it. Also updates `last_graph`.
    pub async fn emit_graph_snapshot(&self, graph: &Graph) {
        let nodes: Vec<NodeDto> = graph
            .nodes
            .values()
            .map(|n| NodeDto::from_node(n, graph.l1.get(&n.id)))
            .collect();
        let edges: Vec<EdgeDto> = graph
            .edges
            .iter()
            .map(EdgeDto::from_edge)
            .collect();
        *self.last_graph.write().await = Arc::new(graph.clone());
        self.emit(RunEvent::GraphSnapshot { nodes, edges });
    }

    /// Wait for the user to provide an answer (via `POST /api/runs/{id}/answer`).
    pub async fn await_answer(&self) -> String {
        self.pending_answer.notified().await;
        self.pending_answer_value
            .lock()
            .await
            .take()
            .unwrap_or_default()
    }

    /// Provide an answer. Called from the HTTP handler.
    pub async fn provide_answer(&self, answer: String) {
        *self.pending_answer_value.lock().await = Some(answer);
        self.pending_answer.notify_one();
    }

    /// Wait for the user to provide a repaired graph (via `POST /api/runs/{id}/repair`).
    pub async fn await_repair(&self) -> Graph {
        self.pending_repair.notified().await;
        self.pending_repair_value
            .lock()
            .await
            .take()
            .unwrap_or_default()
    }

    /// Provide a repaired graph. Called from the HTTP handler.
    pub async fn provide_repair(&self, graph: Graph) {
        *self.pending_repair_value.lock().await = Some(graph);
        self.pending_repair.notify_one();
    }

    /// Snapshot of the run for the `GET /api/runs/{id}` endpoint.
    pub async fn metadata(&self) -> RunMetadata {
        let status = self.status.read().await.clone();
        let duration_ms = self.started_at.elapsed().as_millis() as u64;
        RunMetadata {
            id: self.id.clone(),
            task: self.task.clone(),
            status,
            duration_ms,
            captured_skill: self.captured_skill.read().await.clone(),
        }
    }
}

/// Public-facing run metadata returned by `GET /api/runs/{id}` and
/// `GET /api/runs`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunMetadata {
    pub id: String,
    pub task: String,
    pub status: RunStatus,
    pub duration_ms: u64,
    pub captured_skill: Option<SkillRef>,
}

// RunStatus also needs Serialize/Deserialize for persistence. Add here.
impl serde::Serialize for RunStatus {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Running => s.serialize_newtype_variant("RunStatus", 0, "Running", &()),
            Self::Paused => s.serialize_newtype_variant("RunStatus", 1, "Paused", &()),
            Self::GraphInvalid => s.serialize_newtype_variant("RunStatus", 2, "GraphInvalid", &()),
            Self::Done => s.serialize_newtype_variant("RunStatus", 3, "Done", &()),
            Self::Error(msg) => s.serialize_newtype_variant("RunStatus", 4, "Error", msg),
            Self::Cancelled => s.serialize_newtype_variant("RunStatus", 5, "Cancelled", &()),
        }
    }
}

impl<'de> serde::Deserialize<'de> for RunStatus {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        if let Some(obj) = v.as_object() {
            if obj.contains_key("Running") { return Ok(RunStatus::Running); }
            if obj.contains_key("Paused") { return Ok(RunStatus::Paused); }
            if obj.contains_key("GraphInvalid") { return Ok(RunStatus::GraphInvalid); }
            if obj.contains_key("Done") { return Ok(RunStatus::Done); }
            if let Some(msg) = obj.get("Error").and_then(|v| v.as_str()) {
                return Ok(RunStatus::Error(msg.to_string()));
            }
            if obj.contains_key("Cancelled") { return Ok(RunStatus::Cancelled); }
        }
        // Fallback: treat as Done (safe default for restored runs).
        Ok(RunStatus::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_session_starts_running() {
        let s = RunSession::new("r1".into(), "task".into());
        assert_eq!(*s.status.read().await, RunStatus::Running);
    }

    #[tokio::test]
    async fn emit_and_receive_via_broadcast() {
        let s = RunSession::new("r1".into(), "task".into());
        let mut rx = s.event_tx.subscribe();
        s.emit(RunEvent::Transcript {
            role: "assistant".into(),
            content: "hi".into(),
        });
        let event = rx.recv().await.unwrap();
        match event {
            RunEvent::Transcript { role, content } => {
                assert_eq!(role, "assistant");
                assert_eq!(content, "hi");
            }
            _ => panic!("expected Transcript"),
        }
    }

    #[tokio::test]
    async fn cancel_token_cancels() {
        let s = RunSession::new("r1".into(), "task".into());
        assert!(!s.cancel.is_cancelled());
        s.cancel.cancel();
        assert!(s.cancel.is_cancelled());
    }

    #[tokio::test]
    async fn answer_flow_resolves() {
        let s = std::sync::Arc::new(RunSession::new("r1".into(), "task".into()));
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.await_answer().await });
        // Give the waiter a tick to subscribe.
        tokio::task::yield_now().await;
        s.provide_answer("the answer".into()).await;
        let result = waiter.await.unwrap();
        assert_eq!(result, "the answer");
    }

    #[tokio::test]
    async fn repair_flow_resolves() {
        let s = std::sync::Arc::new(RunSession::new("r1".into(), "task".into()));
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.await_repair().await });
        tokio::task::yield_now().await;
        let g = crate::graph::Graph::new();
        s.provide_repair(g.clone()).await;
        let received = waiter.await.unwrap();
        assert_eq!(received.node_count(), 0);
    }

    #[test]
    fn is_terminal_for_done_and_cancelled() {
        assert!(RunStatus::Done.is_terminal());
        assert!(RunStatus::Cancelled.is_terminal());
        assert!(RunStatus::Error("x".into()).is_terminal());
        assert!(!RunStatus::Running.is_terminal());
        assert!(!RunStatus::Paused.is_terminal());
        assert!(!RunStatus::GraphInvalid.is_terminal());
    }
}
