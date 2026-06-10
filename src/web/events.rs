//! Run event types streamed to the browser over SSE.
//!
//! `RunEvent` is the in-process representation; it's serialized to JSON
//! with a `type` discriminator and forwarded as SSE `event: <type>\ndata: <json>`.

use serde::{Deserialize, Serialize};

/// All events that can be emitted by a running agent. Tagged enum: the
/// outer `type` field identifies the event kind; the inner `data` field
/// carries the payload.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RunEvent {
    /// A new transcript message from the Proposer / SubAgent / Reviewer.
    Transcript { role: String, content: String },
    /// Full snapshot of the graph at this point in time.
    GraphSnapshot { nodes: Vec<NodeDto>, edges: Vec<EdgeDto> },
    /// Loop state transition.
    LoopState { kind: String, payload: serde_json::Value },
    /// Review verdict.
    Review { verdict: String, root_cause: Option<String> },
    /// A skill was captured from a successful run.
    SkillCaptured { slug: String, trigger: String },
    /// Status update — phase transitions, model progress, token
    /// counts. Lightweight; the UI can show "verifying..." or
    /// "running task phase (12k tokens)" without subscribing to the
    /// more verbose loop_state stream.
    Status {
        phase: String,
        message: String,
        tokens_used: u64,
    },
    /// Terminal Done state.
    Done { final_result: serde_json::Value },
    /// An error occurred.
    Error { message: String },
    /// A model call's input and output. Verbose — only sent when detail_mode is on.
    ModelCall {
        component: String,
        model_name: String,
        tier: String,
        request_preview: String,
        response_content: String,
        finish_reason: String,
        prompt_tokens: u64,
        completion_tokens: u64,
        duration_ms: u64,
    },
    /// One step in a cascade backtracking pass.
    CascadeStep {
        changed_node: String,
        predecessor: String,
        depth: usize,
        verdict: String,
        rationale: String,
    },
    /// Lightweight notification that a checkpoint was created.
    CheckpointCreated {
        index: usize,
        round: usize,
        phase: String,
        node_count: usize,
        edge_count: usize,
    },
}

impl RunEvent {
    /// The SSE `event:` field value. Maps to the enum variant name in
    /// snake_case.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::Transcript { .. } => "transcript",
            Self::GraphSnapshot { .. } => "graph",
            Self::LoopState { .. } => "loop_state",
            Self::Review { .. } => "review",
            Self::SkillCaptured { .. } => "skill_captured",
            Self::Status { .. } => "status",
            Self::Done { .. } => "done",
            Self::Error { .. } => "error",
            Self::ModelCall { .. } => "model_call",
            Self::CascadeStep { .. } => "cascade_step",
            Self::CheckpointCreated { .. } => "checkpoint",
        }
    }

    /// Serialize as just the inner `data` payload (without the
    /// `{"type":..., "data":...}` envelope). Used for the SSE `data:`
    /// field, where the event name is already conveyed by the
    /// `event:` field — duplicating it in the JSON would force
    /// consumers to unwrap an extra layer.
    pub fn inner_json(&self) -> serde_json::Result<serde_json::Value> {
        let full = serde_json::to_value(self)?;
        if let serde_json::Value::Object(mut map) = full {
            Ok(map.remove("data").unwrap_or(serde_json::Value::Null))
        } else {
            Ok(full)
        }
    }
}

/// Minimal DTO for a graph node. The full `Node` struct from `crate::graph`
/// is too heavy for SSE; we send only what the UI needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeDto {
    pub id: String,
    pub kind: String,
    pub summary: String,
    pub l1: Option<String>,
    pub l1_confidence: Option<f64>,
}

impl NodeDto {
    pub fn from_node(node: &crate::graph::Node, l1: Option<&crate::graph::L1Description>) -> Self {
        Self {
            id: node.id.to_string(),
            kind: format!("{:?}", node.kind),
            summary: node.summary.clone(),
            l1: l1.map(|d| d.render_oneline()),
            l1_confidence: l1.map(|d| d.confidence),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeDto {
    pub source: String,
    pub target: String,
    pub relation: String,
    pub confidence: f64,
}

impl EdgeDto {
    pub fn from_edge(edge: &crate::graph::Edge) -> Self {
        Self {
            source: edge.source.to_string(),
            target: edge.target.to_string(),
            relation: format!("{:?}", edge.relation),
            confidence: edge.confidence,
        }
    }
}

/// Compact DTO for "seed the next run with the graph state I already
/// have in the browser". The frontend only has `NodeDto`/`EdgeDto`
/// from the last `graph` SSE event, not the full `Graph` (which has
/// L1 store + indices + metadata). This DTO lets the frontend send
/// the L0 skeleton and the server reconstruct a minimal `Graph`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InitialGraphDto {
    #[serde(default)]
    pub nodes: Vec<NodeDto>,
    #[serde(default)]
    pub edges: Vec<EdgeDto>,
}

impl InitialGraphDto {
    /// Build a DTO from an existing Graph (for branch creation).
    pub fn from_graph(g: &crate::graph::Graph) -> Self {
        let nodes: Vec<NodeDto> = g
            .nodes
            .values()
            .map(|n| NodeDto::from_node(n, g.l1.get(&n.id)))
            .collect();
        let edges: Vec<EdgeDto> = g.edges.iter().map(EdgeDto::from_edge).collect();
        Self { nodes, edges }
    }

    /// Build a `Graph` from the L0 skeleton. The reconstructed graph
    /// has no L1 (those are re-derived by the enricher on any new
    /// patch the Proposer proposes), no metadata, and edge indices
    /// are rebuilt after insertion.
    pub fn into_graph(self) -> crate::graph::Graph {
        use crate::graph::{Node, NodeKind, RelationType};
        let mut g = crate::graph::Graph::new();
        for n in self.nodes {
            let kind = NodeKind::parse_wire(&n.kind);
            let node = Node::new(n.id.clone(), kind, n.id.clone(), n.summary);
            g.add_node(node);
        }
        for e in self.edges {
            let relation = RelationType::parse_wire(&e.relation);
            let edge = crate::graph::Edge::new(
                e.source,
                e.target,
                relation,
                e.confidence,
                String::new(),
            );
            // Tolerate dangling endpoints from stale snapshots — we
            // still want to preserve the surviving structure.
            let _ = g.add_edge(edge);
        }
        g
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_event_serializes_with_type_discriminator() {
        let event = RunEvent::Transcript {
            role: "assistant".into(),
            content: "hello".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "transcript");
        assert_eq!(v["data"]["role"], "assistant");
        assert_eq!(v["data"]["content"], "hello");
    }

    #[test]
    fn run_event_event_name_matches_variant() {
        assert_eq!(RunEvent::Transcript { role: "x".into(), content: "y".into() }.event_name(), "transcript");
        assert_eq!(RunEvent::Done { final_result: serde_json::json!({}) }.event_name(), "done");
        assert_eq!(RunEvent::Error { message: "x".into() }.event_name(), "error");
    }

    #[test]
    fn node_dto_omits_heavy_fields() {
        let node = crate::graph::Node::file("a.rs", "a file");
        let dto = NodeDto::from_node(&node, None);
        assert_eq!(dto.id, "a.rs");
        assert_eq!(dto.summary, "a file");
        assert!(dto.l1.is_none());
    }

    #[test]
    fn edge_dto_serializes_source_target_relation() {
        let edge = crate::graph::Edge::new("a", "b", crate::graph::RelationType::Imports, 0.9, "");
        let dto = EdgeDto::from_edge(&edge);
        assert_eq!(dto.source, "a");
        assert_eq!(dto.target, "b");
        assert!(dto.relation.contains("Imports"));
        assert!((dto.confidence - 0.9).abs() < 1e-9);
    }

    #[test]
    fn inner_json_strips_envelope_for_sse() {
        // SSE consumers see the event type via the `event:` field, so
        // the `data:` field should be the inner payload only —
        // otherwise consumers have to unwrap `parsed.data` everywhere.
        let event = RunEvent::Transcript {
            role: "assistant".into(),
            content: "hello".into(),
        };
        let v = event.inner_json().unwrap();
        // No `type` key in the inner payload.
        assert!(v.get("type").is_none());
        // Inner fields are at the top level.
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"], "hello");
    }

    #[test]
    fn initial_graph_dto_into_graph_rebuilds_skeleton() {
        // Simulates a frontend that has tracked two nodes + an edge
        // through earlier SSE events and is now resending them as
        // the seed for a new conversation turn.
        let dto = InitialGraphDto {
            nodes: vec![
                NodeDto {
                    id: "x".into(),
                    kind: "File".into(),
                    summary: "module X".into(),
                    l1: None,
                    l1_confidence: None,
                },
                NodeDto {
                    id: "y".into(),
                    kind: "Module".into(),
                    summary: "module Y".into(),
                    l1: None,
                    l1_confidence: None,
                },
            ],
            edges: vec![EdgeDto {
                source: "x".into(),
                target: "y".into(),
                relation: "Imports".into(),
                confidence: 0.8,
            }],
        };
        let g = dto.into_graph();
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
        assert!(g.get_node(&crate::graph::NodeId::from("x")).is_some());
        assert!(g.get_node(&crate::graph::NodeId::from("y")).is_some());
        // Indices must be rebuilt so subsequent ops (apply_patch,
        // outgoing/incoming iterators) work.
        let x = crate::graph::NodeId::from("x");
        let y = crate::graph::NodeId::from("y");
        let outs: Vec<&crate::graph::NodeId> = g.neighbors(&x).collect();
        assert_eq!(outs, vec![&y]);
    }

    #[test]
    fn initial_graph_dto_empty_yields_empty_graph() {
        let g = InitialGraphDto::default().into_graph();
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn initial_graph_dto_tolerates_dangling_endpoints() {
        // Edge with a target that isn't in the DTO's node list. Should
        // not panic — the graph stays empty for that edge.
        let dto = InitialGraphDto {
            nodes: vec![NodeDto {
                id: "a".into(),
                kind: "File".into(),
                summary: "A".into(),
                l1: None,
                l1_confidence: None,
            }],
            edges: vec![EdgeDto {
                source: "a".into(),
                target: "ghost".into(),
                relation: "Imports".into(),
                confidence: 0.5,
            }],
        };
        let g = dto.into_graph();
        assert_eq!(g.node_count(), 1);
        assert_eq!(g.edge_count(), 0, "dangling edge should be silently dropped");
    }
}
