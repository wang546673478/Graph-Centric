//! Run event types streamed to the browser over SSE.
//!
//! `RunEvent` is the in-process representation; it's serialized to JSON
//! with a `type` discriminator and forwarded as SSE `event: <type>\ndata: <json>`.

use serde::Serialize;

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
    /// Terminal Done state.
    Done { final_result: serde_json::Value },
    /// An error occurred.
    Error { message: String },
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
            Self::Done { .. } => "done",
            Self::Error { .. } => "error",
        }
    }
}

/// Minimal DTO for a graph node. The full `Node` struct from `crate::graph`
/// is too heavy for SSE; we send only what the UI needs.
#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
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
}
