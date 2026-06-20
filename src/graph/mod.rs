//! Core graph types — the foundation of [[project-graph-centric]].
//!
//! This module defines the **world graph**: nodes are entities (files,
//! functions, services, datasets, concepts) and edges are real structural
//! relationships. The same machinery is reused for the **task DAG** that
//! drives the scheduler — see `crate::scheduler`.
//!
//! Per design principle #4 (universality lives in the model, structure
//! lives in the graph), no domain-specific types live here. Languages,
//! frameworks, and other domain extras go into `Node::metadata`.

use crate::error::{HarnessError, Result};
use serde::de::{Deserializer, MapAccess, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

pub mod l1;
pub mod traversal;
pub mod validation;

pub use l1::{L1Description, L1Store};
pub use traversal::Traversal;
pub use validation::Inconsistency;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Stable identifier for a node. We use path-like human-readable strings so
/// that the graph remains debuggable in logs and JSON dumps.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl NodeId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for NodeId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for NodeId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Enums — kept deliberately generic
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// A source file on disk (Rust, TS, CSS, etc.).
    File,
    /// A function definition.
    Function,
    /// A class or struct definition.
    Class,
    /// A module or directory grouping.
    Module,
    /// A configuration file or block.
    Config,
    /// A task in a plan DAG — one unit of work.
    Task,
    /// A UI component (Vue SFC, React component, Web Component, etc.).
    Component,
    /// A visual style definition (CSS block, theme token, design variable).
    Style,
    /// A layout definition (grid, flexbox, page structure).
    Layout,
    /// A page or route — one screen in a web app.
    Page,
    /// Free-form kind for domain-specific node types.
    Other(String),
}

impl NodeKind {
    /// Canonical wire-form string for this kind. Canonical variants
    /// (`File`, `Function`, …) return their name; `Other(s)` returns `s`.
    pub fn as_wire(&self) -> &str {
        match self {
            Self::File => "File",
            Self::Function => "Function",
            Self::Class => "Class",
            Self::Module => "Module",
            Self::Config => "Config",
            Self::Task => "Task",
            Self::Component => "Component",
            Self::Style => "Style",
            Self::Layout => "Layout",
            Self::Page => "Page",
            Self::Other(s) => s.as_str(),
        }
    }

    /// Parse a wire string into the appropriate variant. Canonical names
    /// route to the matching variant; everything else becomes `Other(s)`.
    /// Note: this means `parse_wire("File")` always returns `NodeKind::File`,
    /// even if the caller meant `NodeKind::Other("File")` (acceptable —
    /// custom kinds shouldn't collide with canonical names).
    pub fn parse_wire(s: &str) -> Self {
        match s {
            "File" => Self::File,
            "Function" => Self::Function,
            "Class" => Self::Class,
            "Module" => Self::Module,
            "Config" => Self::Config,
            "Task" => Self::Task,
            "Component" => Self::Component,
            "Style" => Self::Style,
            "Layout" => Self::Layout,
            "Page" => Self::Page,
            _ => Self::Other(s.to_string()),
        }
    }
}

impl Serialize for NodeKind {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_wire())
    }
}

impl<'de> Deserialize<'de> for NodeKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = NodeKind;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a NodeKind: a string (preferred) or a legacy single-key object")
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> std::result::Result<Self::Value, E> {
                Ok(NodeKind::parse_wire(s))
            }
            fn visit_string<E: serde::de::Error>(self, s: String) -> std::result::Result<Self::Value, E> {
                Ok(NodeKind::parse_wire(&s))
            }

            /// Legacy form: `{"File": null}` or `{"Other": "x"}` (Rust's
            /// default tuple-variant serde shape). Accepted for round-trips
            /// of old saved graphs; new writes always use the string form.
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let entry: Option<(String, serde_json::Value)> = map.next_entry()?;
                let (variant, value) =
                    entry.ok_or_else(|| serde::de::Error::custom("empty NodeKind object"))?;
                // Reject objects with multiple keys to keep the form unambiguous.
                if map.next_key::<String>()?.is_some() {
                    return Err(serde::de::Error::custom(
                        "NodeKind object must have exactly one key",
                    ));
                }
                Ok(match variant.as_str() {
                    "File" => NodeKind::File,
                    "Function" => NodeKind::Function,
                    "Class" => NodeKind::Class,
                    "Module" => NodeKind::Module,
                    "Config" => NodeKind::Config,
                    "Task" => NodeKind::Task,
                    "Other" => match value {
                        serde_json::Value::String(s) => NodeKind::Other(s),
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "Other variant expects a string value, got {}",
                                other
                            )));
                        }
                    },
                    other => NodeKind::Other(other.to_string()),
                })
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RelationType {
    // Structural
    Contains,
    BelongsTo,
    // Dependency
    Imports,
    Exports,
    DependsOn,
    // Flow — process / sequencing ("start leads to deliverable"). May cycle.
    LeadsTo,
    // Behavioral
    Calls,
    Triggers,
    // Data
    Reads,
    Writes,
    // Meta — provenance of beliefs about the graph itself
    RevealedBy,
    InvalidatedBy,
    // Escape hatch
    Other(String),
}

impl RelationType {
    pub fn as_wire(&self) -> &str {
        match self {
            Self::Contains => "Contains",
            Self::BelongsTo => "BelongsTo",
            Self::Imports => "Imports",
            Self::Exports => "Exports",
            Self::DependsOn => "DependsOn",
            Self::LeadsTo => "LeadsTo",
            Self::Calls => "Calls",
            Self::Triggers => "Triggers",
            Self::Reads => "Reads",
            Self::Writes => "Writes",
            Self::RevealedBy => "RevealedBy",
            Self::InvalidatedBy => "InvalidatedBy",
            Self::Other(s) => s.as_str(),
        }
    }

    pub fn parse_wire(s: &str) -> Self {
        match s {
            "Contains" => Self::Contains,
            "BelongsTo" => Self::BelongsTo,
            "Imports" => Self::Imports,
            "Exports" => Self::Exports,
            "DependsOn" => Self::DependsOn,
            "LeadsTo" => Self::LeadsTo,
            "Calls" => Self::Calls,
            "Triggers" => Self::Triggers,
            "Reads" => Self::Reads,
            "Writes" => Self::Writes,
            "RevealedBy" => Self::RevealedBy,
            "InvalidatedBy" => Self::InvalidatedBy,
            _ => Self::Other(s.to_string()),
        }
    }

    /// Structural relations participate in graph connectivity/replay
    /// traversal (start → deliverable flow, dependencies, containment).
    /// Meta-relations (provenance) do not. Used by path_exists/replay to
    /// decide which edges are walkable.
    pub fn is_structural(&self) -> bool {
        matches!(
            self,
            Self::Contains | Self::BelongsTo | Self::Imports | Self::Exports
                | Self::DependsOn | Self::LeadsTo | Self::Calls | Self::Triggers
                | Self::Reads | Self::Writes
        )
    }
}

impl Serialize for RelationType {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_wire())
    }
}

impl<'de> Deserialize<'de> for RelationType {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RelationType;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a RelationType: a string (preferred) or a legacy single-key object")
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> std::result::Result<Self::Value, E> {
                Ok(RelationType::parse_wire(s))
            }
            fn visit_string<E: serde::de::Error>(self, s: String) -> std::result::Result<Self::Value, E> {
                Ok(RelationType::parse_wire(&s))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let entry: Option<(String, serde_json::Value)> = map.next_entry()?;
                let (variant, value) =
                    entry.ok_or_else(|| serde::de::Error::custom("empty RelationType object"))?;
                if map.next_key::<String>()?.is_some() {
                    return Err(serde::de::Error::custom(
                        "RelationType object must have exactly one key",
                    ));
                }
                Ok(match variant.as_str() {
                    "Contains" => RelationType::Contains,
                    "BelongsTo" => RelationType::BelongsTo,
                    "Imports" => RelationType::Imports,
                    "Exports" => RelationType::Exports,
                    "DependsOn" => RelationType::DependsOn,
                    "Calls" => RelationType::Calls,
                    "Triggers" => RelationType::Triggers,
                    "Reads" => RelationType::Reads,
                    "Writes" => RelationType::Writes,
                    "RevealedBy" => RelationType::RevealedBy,
                    "InvalidatedBy" => RelationType::InvalidatedBy,
                    "Other" => match value {
                        serde_json::Value::String(s) => RelationType::Other(s),
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "Other variant expects a string value, got {}",
                                other
                            )));
                        }
                    },
                    other => RelationType::Other(other.to_string()),
                })
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphStatus {
    /// Just constructed, never verified.
    Draft,
    /// Has passed structural + sampled-against-source verification.
    Verified,
    /// Verified for some subgraph; the rest is Draft.
    Partial,
    /// Known to be wrong; repair pending.
    Invalidated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeChangeKind {
    Created,
    ConfidenceUpdated,
    EvidenceAppended,
    Invalidated,
    Repaired,
    Removed,
}

// ---------------------------------------------------------------------------
// Node & Edge
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    pub path: String,
    pub summary: String,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
    /// If true, this node is the anchor (user's immutable intent) and
    /// must never be removed or have its kind/path/summary changed by
    /// any repair or re-plan operation. Cascade backtracking stops at
    /// this node.
    #[serde(default)]
    pub immutable: bool,
    /// If true, this node has been expanded into a sub-graph (fractal
    /// architecture: complex nodes contain their own L0/L1/L2).
    #[serde(default)]
    pub expanded: bool,
}

impl Node {
    pub fn new(
        id: impl Into<NodeId>,
        kind: NodeKind,
        path: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            path: path.into(),
            summary: summary.into(),
            metadata: HashMap::new(),
            immutable: false,
            expanded: false,
        }
    }

    /// Convenience: a file node where the id == the path.
    pub fn file(path: impl Into<String>, summary: impl Into<String>) -> Self {
        let path = path.into();
        Self::new(path.clone(), NodeKind::File, path, summary)
    }

    /// Convenience: a function node where the id == the fully-qualified name.
    pub fn function(qualified_name: impl Into<String>, summary: impl Into<String>) -> Self {
        let q = qualified_name.into();
        Self::new(q.clone(), NodeKind::Function, q, summary)
    }

    /// Convenience: a task node for the task DAG.
    pub fn task(id: impl Into<String>, description: impl Into<String>) -> Self {
        let id = id.into();
        Self::new(
            id.clone(),
            NodeKind::Task,
            format!("task:{id}"),
            description,
        )
    }

    /// Convenience: a UI component node (Vue SFC, React component, etc.).
    pub fn component(id: impl Into<String>, summary: impl Into<String>) -> Self {
        let id = id.into();
        Self::new(id.clone(), NodeKind::Component, id, summary)
    }

    /// Convenience: a visual style node (CSS block, theme token, design variable).
    pub fn style(id: impl Into<String>, summary: impl Into<String>) -> Self {
        let id = id.into();
        Self::new(id.clone(), NodeKind::Style, id, summary)
    }

    /// Convenience: a layout node (grid, flexbox, page structure).
    pub fn layout(id: impl Into<String>, summary: impl Into<String>) -> Self {
        let id = id.into();
        Self::new(id.clone(), NodeKind::Layout, id, summary)
    }

    /// Convenience: a page or route node (one screen in a web app).
    pub fn page(id: impl Into<String>, summary: impl Into<String>) -> Self {
        let id = id.into();
        Self::new(id.clone(), NodeKind::Page, id, summary)
    }

    pub fn with_metadata(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub source: NodeId,
    pub target: NodeId,
    pub relation: RelationType,
    pub confidence: f64,
    pub evidence: String,
    #[serde(default)]
    pub history: Vec<EdgeChange>,
}

impl Edge {
    pub fn new(
        source: impl Into<NodeId>,
        target: impl Into<NodeId>,
        relation: RelationType,
        confidence: f64,
        evidence: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            relation,
            confidence: confidence.clamp(0.0, 1.0),
            evidence: evidence.into(),
            history: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeChange {
    pub at_version: usize,
    pub kind: EdgeChangeKind,
    pub reason: String,
}

/// Patch payload used by graph-repair and proposer flows.
///
/// In addition to L0 (node/edge) mutations, a patch can also write L1
/// descriptions (`set_l1`) — used by the L1Semantic repair path. The
/// `Graph::apply_patch` method applies all four sections in a fixed
/// order: add_nodes → add_edges → remove edges (by descending index) →
/// remove nodes → set_l1.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphPatch {
    pub add_nodes: Vec<Node>,
    pub add_edges: Vec<Edge>,
    pub remove_node_ids: Vec<NodeId>,
    pub remove_edge_indices: Vec<usize>,
    /// L1 description updates: `(node_id, new_description)`. Applied after
    /// node/edge mutations so any newly-added nodes can receive L1 in the
    /// same patch.
    #[serde(default)]
    pub set_l1: Vec<(NodeId, L1Description)>,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Graph
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: HashMap<NodeId, Node>,
    pub edges: Vec<Edge>,
    /// L1 store — per-node semantic descriptions. Lives alongside L0
    /// nodes/edges per the three-layer model (see [`l1`] doc and
    /// [[feedback-three-layer-graph]]).
    #[serde(default)]
    pub l1: L1Store,
    #[serde(skip)]
    outgoing_idx: HashMap<NodeId, Vec<usize>>,
    #[serde(skip)]
    incoming_idx: HashMap<NodeId, Vec<usize>>,
    pub version: usize,
    pub status: GraphStatus,
    /// Fractal architecture: if this graph is a complex node's sub-graph,
    /// parent holds the (parent_node_id, parent_graph). Not serialized —
    /// reconstructed on load by re-walking the parent's Contains edges.
    #[serde(skip)]
    pub parent: Option<(NodeId, Box<Graph>)>,
}

impl Default for Graph {
    fn default() -> Self {
        Self {
            nodes: HashMap::new(),
            edges: Vec::new(),
            l1: L1Store::new(),
            outgoing_idx: HashMap::new(),
            incoming_idx: HashMap::new(),
            version: 0,
            status: GraphStatus::Draft,
            parent: None,
        }
    }
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or replace) a node. Returns its id.
    pub fn add_node(&mut self, node: Node) -> NodeId {
        let id = node.id.clone();
        self.nodes.insert(id.clone(), node);
        id
    }

    /// Add an edge. Both endpoints must already exist as nodes — this is the
    /// first determinism guarantee: no dangling edges by construction.
    pub fn add_edge(&mut self, edge: Edge) -> Result<()> {
        if !self.nodes.contains_key(&edge.source) {
            return Err(HarnessError::graph(format!(
                "source node missing: {}",
                edge.source
            )));
        }
        if !self.nodes.contains_key(&edge.target) {
            return Err(HarnessError::graph(format!(
                "target node missing: {}",
                edge.target
            )));
        }
        let idx = self.edges.len();
        self.outgoing_idx
            .entry(edge.source.clone())
            .or_default()
            .push(idx);
        self.incoming_idx
            .entry(edge.target.clone())
            .or_default()
            .push(idx);
        self.edges.push(edge);
        Ok(())
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
    pub fn contains_node(&self, id: &NodeId) -> bool {
        self.nodes.contains_key(id)
    }
    pub fn get_node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }
    pub fn get_node_mut(&mut self, id: &NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(id)
    }
    pub fn get_edge(&self, idx: usize) -> Option<&Edge> {
        self.edges.get(idx)
    }

    /// Iterate all nodes (order not guaranteed).
    pub fn iter_nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    /// Iterate all edges in insertion order.
    pub fn iter_edges(&self) -> impl Iterator<Item = &Edge> {
        self.edges.iter()
    }

    /// Edges with `id` as their source.
    pub fn outgoing(&self, id: &NodeId) -> impl Iterator<Item = &Edge> {
        let indices = self.outgoing_idx.get(id);
        indices
            .into_iter()
            .flatten()
            .filter_map(move |&i| self.edges.get(i))
    }

    /// Edges with `id` as their target.
    pub fn incoming(&self, id: &NodeId) -> impl Iterator<Item = &Edge> {
        let indices = self.incoming_idx.get(id);
        indices
            .into_iter()
            .flatten()
            .filter_map(move |&i| self.edges.get(i))
    }

    /// Direct neighbors following outgoing edges.
    pub fn neighbors<'a>(&'a self, id: &'a NodeId) -> impl Iterator<Item = &'a NodeId> + 'a {
        self.outgoing(id).map(|e| &e.target)
    }

    /// Direct predecessors via incoming edges.
    pub fn predecessors<'a>(&'a self, id: &'a NodeId) -> impl Iterator<Item = &'a NodeId> + 'a {
        self.incoming(id).map(|e| &e.source)
    }

    pub fn bump_version(&mut self) -> usize {
        self.version += 1;
        self.version
    }

    /// Apply a patch — used by graph repair flows. Local by design (principle #3).
    pub fn apply_patch(&mut self, patch: GraphPatch) -> Result<()> {
        for node in patch.add_nodes {
            self.add_node(node);
        }
        for edge in patch.add_edges {
            self.add_edge(edge)?;
        }
        // Removal is order-sensitive; do edges before nodes
        let mut to_remove = patch.remove_edge_indices;
        to_remove.sort_unstable_by(|a, b| b.cmp(a));
        for idx in to_remove {
            if idx < self.edges.len() {
                self.edges.remove(idx);
            }
        }
        for id in patch.remove_node_ids {
            self.nodes.remove(&id);
        }
        self.rebuild_indices();
        // L1 updates apply last so they can target nodes added earlier in
        // this same patch.
        for (id, desc) in patch.set_l1 {
            if self.nodes.contains_key(&id) {
                self.l1.set(id, desc);
            }
            // Silently drop set_l1 entries for nodes that don't exist —
            // they're stale relative to the post-mutation graph.
        }
        self.bump_version();
        Ok(())
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(HarnessError::from)
    }

    pub fn from_json(s: &str) -> Result<Self> {
        let mut g: Self = serde_json::from_str(s)?;
        g.rebuild_indices();
        Ok(g)
    }

    pub(crate) fn rebuild_indices(&mut self) {
        self.outgoing_idx.clear();
        self.incoming_idx.clear();
        for (idx, edge) in self.edges.iter().enumerate() {
            self.outgoing_idx
                .entry(edge.source.clone())
                .or_default()
                .push(idx);
            self.incoming_idx
                .entry(edge.target.clone())
                .or_default()
                .push(idx);
        }
    }

    // Insert raw — only used internally by traversal/subgraph extraction
    // where index rebuild happens at the end.
    pub(crate) fn insert_node_raw(&mut self, node: Node) {
        self.nodes.insert(node.id.clone(), node);
    }

    pub(crate) fn insert_edge_raw(&mut self, edge: Edge) {
        self.edges.push(edge);
    }

    /// Return all edges where `node` is the target, paired with the source node.
    /// This is the inverse of the natural edge direction — "which nodes point TO me?"
    pub fn predecessors_of(&self, node: &NodeId) -> Vec<(&Edge, &Node)> {
        self.incoming(node)
            .filter_map(|e| self.nodes.get(&e.source).map(|n| (e, n)))
            .collect()
    }

    /// Walk inbound edges from `start` toward the anchor. Returns the ordered
    /// path from the farthest ancestor to `start` (includes the anchor itself
    /// if reachable). Uses BFS on reversed edges; stops when an immutable node
    /// is reached.
    pub fn path_to_anchor(&self, start: &NodeId) -> Vec<NodeId> {
        let mut path = Vec::new();
        let mut current = start.clone();
        // Safety cap: max 1000 hops.
        for _ in 0..1000 {
            let preds: Vec<NodeId> = self.predecessors(&current).cloned().collect();
            if preds.is_empty() {
                break;
            }
            // Prefer the first predecessor that is the anchor; otherwise follow
            // the first predecessor. For DAG nodes with multiple inbound edges,
            // callers should use `predecessors_of()` directly to handle branches.
            if let Some(anchor) = preds.iter().find(|id| {
                self.nodes.get(id).map(|n| n.immutable).unwrap_or(false)
            }) {
                path.push(anchor.clone());
                break;
            }
            path.push(preds[0].clone());
            current = preds[0].clone();
        }
        // Path was built from start backward; reverse to get root→leaf order.
        path.reverse();
        path
    }

    /// Mark a node as the immutable anchor. Panics if the node doesn't exist
    /// (caller should check first via `contains_node`).
    pub fn set_anchor(&mut self, id: &NodeId) {
        if let Some(node) = self.nodes.get_mut(id) {
            node.immutable = true;
        }
    }

    /// Return the anchor node, if one has been set.
    pub fn anchor(&self) -> Option<&Node> {
        self.nodes.values().find(|n| n.immutable)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn two_node_graph() -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::file("a.rs", "A"));
        g.add_node(Node::file("b.rs", "B"));
        g.add_edge(Edge::new(
            "a.rs",
            "b.rs",
            RelationType::Imports,
            1.0,
            "use crate::b",
        ))
        .unwrap();
        g
    }

    #[test]
    fn counts_match_after_construction() {
        let g = two_node_graph();
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
    }

    #[test]
    fn rejects_edge_with_missing_source() {
        let mut g = Graph::new();
        g.add_node(Node::file("a.rs", "A"));
        let err = g
            .add_edge(Edge::new(
                "missing",
                "a.rs",
                RelationType::Imports,
                1.0,
                "x",
            ))
            .unwrap_err();
        assert!(format!("{err}").contains("source node missing"));
    }

    #[test]
    fn rejects_edge_with_missing_target() {
        let mut g = Graph::new();
        g.add_node(Node::file("a.rs", "A"));
        let err = g
            .add_edge(Edge::new(
                "a.rs",
                "missing",
                RelationType::Imports,
                1.0,
                "x",
            ))
            .unwrap_err();
        assert!(format!("{err}").contains("target node missing"));
    }

    #[test]
    fn neighbors_and_predecessors() {
        let g = two_node_graph();
        let a = NodeId::from("a.rs");
        let b = NodeId::from("b.rs");
        let outs: Vec<&NodeId> = g.neighbors(&a).collect();
        assert_eq!(outs, vec![&b]);
        let preds: Vec<&NodeId> = g.predecessors(&b).collect();
        assert_eq!(preds, vec![&a]);
    }

    #[test]
    fn confidence_clamped_to_unit_interval() {
        let e = Edge::new("a", "b", RelationType::Calls, 1.7, "");
        assert!((e.confidence - 1.0).abs() < f64::EPSILON);
        let e = Edge::new("a", "b", RelationType::Calls, -0.3, "");
        assert!(e.confidence.abs() < f64::EPSILON);
    }

    #[test]
    fn json_round_trip_rebuilds_indices() {
        let g = two_node_graph();
        let s = g.to_json().unwrap();
        let g2 = Graph::from_json(&s).unwrap();
        assert_eq!(g2.node_count(), 2);
        assert_eq!(g2.edge_count(), 1);
        // Indices must work after rebuild
        let a = NodeId::from("a.rs");
        assert_eq!(g2.neighbors(&a).count(), 1);
    }

    #[test]
    fn apply_patch_adds_and_removes() {
        let mut g = two_node_graph();
        let v0 = g.version;
        let patch = GraphPatch {
            add_nodes: vec![Node::file("c.rs", "C")],
            add_edges: vec![Edge::new("b.rs", "c.rs", RelationType::Calls, 0.9, "")],
            remove_node_ids: vec![],
            remove_edge_indices: vec![],
            set_l1: vec![],
            reason: "test patch".into(),
        };
        g.apply_patch(patch).unwrap();
        assert_eq!(g.node_count(), 3);
        assert_eq!(g.edge_count(), 2);
        assert_eq!(g.version, v0 + 1);
    }

    #[test]
    fn apply_patch_writes_l1_for_added_nodes() {
        let mut g = two_node_graph();
        let patch = GraphPatch {
            add_nodes: vec![Node::file("c.rs", "C summary")],
            add_edges: vec![],
            remove_node_ids: vec![],
            remove_edge_indices: vec![],
            set_l1: vec![(
                NodeId::from("c.rs"),
                L1Description::new("does C", "wraps lib", "intent", "constraint"),
            )],
            reason: "add C with L1".into(),
        };
        g.apply_patch(patch).unwrap();
        let l1 = g.l1.get(&NodeId::from("c.rs")).unwrap();
        assert_eq!(l1.responsibility, "does C");
    }

    #[test]
    fn apply_patch_skips_l1_for_unknown_nodes() {
        let mut g = two_node_graph();
        let patch = GraphPatch {
            set_l1: vec![(
                NodeId::from("ghost"),
                L1Description::new("ghost L1", "", "", ""),
            )],
            ..Default::default()
        };
        g.apply_patch(patch).unwrap();
        // ghost wasn't added; L1 entry should not be persisted
        assert!(g.l1.get(&NodeId::from("ghost")).is_none());
    }

    #[test]
    fn node_metadata_attached() {
        let n = Node::file("x.rs", "X").with_metadata("language", serde_json::json!("rust"));
        assert_eq!(n.metadata.get("language").unwrap(), &serde_json::json!("rust"));
    }

    // ---- NodeKind / RelationType serde ----

    #[test]
    fn node_kind_canonical_serializes_as_plain_string() {
        assert_eq!(serde_json::to_string(&NodeKind::File).unwrap(), r#""File""#);
        assert_eq!(serde_json::to_string(&NodeKind::Module).unwrap(), r#""Module""#);
        assert_eq!(serde_json::to_string(&NodeKind::Task).unwrap(), r#""Task""#);
    }

    #[test]
    fn node_kind_other_serializes_as_inner_string() {
        let k = NodeKind::Other("database".to_string());
        assert_eq!(serde_json::to_string(&k).unwrap(), r#""database""#);
    }

    #[test]
    fn node_kind_deserializes_canonical_string() {
        let k: NodeKind = serde_json::from_str(r#""File""#).unwrap();
        assert!(matches!(k, NodeKind::File));
        let k: NodeKind = serde_json::from_str(r#""Module""#).unwrap();
        assert!(matches!(k, NodeKind::Module));
    }

    #[test]
    fn node_kind_deserializes_unknown_string_as_other() {
        let k: NodeKind = serde_json::from_str(r#""database""#).unwrap();
        match k {
            NodeKind::Other(s) => assert_eq!(s, "database"),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn node_kind_deserializes_legacy_object_form() {
        // Backwards-compat: old graph.json files used Rust's default
        // tuple-variant shape {"Other":"X"} / {"File": null}.
        let k: NodeKind = serde_json::from_str(r#"{"Other":"BoardMeeting"}"#).unwrap();
        match k {
            NodeKind::Other(s) => assert_eq!(s, "BoardMeeting"),
            other => panic!("expected Other, got {other:?}"),
        }
        let k: NodeKind = serde_json::from_str(r#"{"File":null}"#).unwrap();
        assert!(matches!(k, NodeKind::File));
        // Even the {"Other":"Other"} corner case from the demo output round-trips.
        let k: NodeKind = serde_json::from_str(r#"{"Other":"Other"}"#).unwrap();
        match k {
            NodeKind::Other(s) => assert_eq!(s, "Other"),
            other => panic!("expected Other(\"Other\"), got {other:?}"),
        }
    }

    #[test]
    fn node_kind_rejects_multi_key_object() {
        let r: std::result::Result<NodeKind, _> =
            serde_json::from_str(r#"{"File": null, "Other": "x"}"#);
        assert!(r.is_err());
    }

    #[test]
    fn relation_type_canonical_serializes_as_plain_string() {
        assert_eq!(
            serde_json::to_string(&RelationType::DependsOn).unwrap(),
            r#""DependsOn""#
        );
        assert_eq!(
            serde_json::to_string(&RelationType::Imports).unwrap(),
            r#""Imports""#
        );
    }

    #[test]
    fn relation_type_other_serializes_as_inner_string() {
        let r = RelationType::Other("soft_coupling".to_string());
        assert_eq!(serde_json::to_string(&r).unwrap(), r#""soft_coupling""#);
    }

    #[test]
    fn relation_type_deserializes_canonical_and_other() {
        let r: RelationType = serde_json::from_str(r#""DependsOn""#).unwrap();
        assert!(matches!(r, RelationType::DependsOn));
        let r: RelationType = serde_json::from_str(r#""soft_coupling""#).unwrap();
        match r {
            RelationType::Other(s) => assert_eq!(s, "soft_coupling"),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn relation_type_deserializes_legacy_object_form() {
        let r: RelationType = serde_json::from_str(r#"{"DependsOn":null}"#).unwrap();
        assert!(matches!(r, RelationType::DependsOn));
        let r: RelationType = serde_json::from_str(r#"{"Other":"weak_link"}"#).unwrap();
        match r {
            RelationType::Other(s) => assert_eq!(s, "weak_link"),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn full_graph_json_round_trip_with_kinds_and_relations() {
        // End-to-end check: a graph with mixed canonical and Other variants
        // serializes cleanly and deserializes back to the same logical state.
        let mut g = Graph::new();
        g.add_node(Node::new("a", NodeKind::File, "a.rs", "file A"));
        g.add_node(Node::new(
            "db",
            NodeKind::Other("database".into()),
            "ext:db",
            "Postgres",
        ));
        g.add_edge(Edge::new("a", "db", RelationType::DependsOn, 0.9, "uses db"))
            .unwrap();
        g.add_edge(Edge::new(
            "a",
            "db",
            RelationType::Other("soft_coupling".into()),
            0.5,
            "loose",
        ))
        .unwrap();

        let json = g.to_json().unwrap();
        // The JSON should use the new clean form, not the old object form.
        assert!(json.contains(r#""kind": "File""#));
        assert!(json.contains(r#""kind": "database""#));
        assert!(json.contains(r#""relation": "DependsOn""#));
        assert!(json.contains(r#""relation": "soft_coupling""#));
        assert!(!json.contains(r#"{"Other""#), "JSON still contains object Other form:\n{json}");

        let g2 = Graph::from_json(&json).unwrap();
        assert_eq!(g2.node_count(), 2);
        assert_eq!(g2.edge_count(), 2);
        assert!(matches!(
            g2.get_node(&NodeId::from("a")).unwrap().kind,
            NodeKind::File
        ));
        match &g2.get_node(&NodeId::from("db")).unwrap().kind {
            NodeKind::Other(s) => assert_eq!(s, "database"),
            other => panic!("expected Other(\"database\"), got {other:?}"),
        }
    }

    #[test]
    fn predecessors_of_returns_inbound_edges() {
        let mut g = Graph::new();
        g.add_node(Node::task("a", "anchor"));
        g.add_node(Node::task("b", "child"));
        g.add_edge(Edge::new("a", "b", RelationType::DependsOn, 1.0, "")).unwrap();
        let preds = g.predecessors_of(&NodeId::from("b"));
        assert_eq!(preds.len(), 1);
        assert_eq!(preds[0].1.id.as_str(), "a");
    }

    #[test]
    fn path_to_anchor_stops_at_immutable() {
        let mut g = Graph::new();
        let mut anchor = Node::task("a", "anchor");
        anchor.immutable = true;
        g.add_node(anchor);
        g.add_node(Node::task("b", "mid"));
        g.add_node(Node::task("c", "leaf"));
        g.add_edge(Edge::new("a", "b", RelationType::DependsOn, 1.0, "")).unwrap();
        g.add_edge(Edge::new("b", "c", RelationType::DependsOn, 1.0, "")).unwrap();
        let path = g.path_to_anchor(&NodeId::from("c"));
        // path_to_anchor returns farthest-ancestor-first: anchor → ... → parent-of-start
        assert_eq!(path, vec![NodeId::from("a"), NodeId::from("b")]);
    }

    #[test]
    fn set_anchor_marks_node_immutable() {
        let mut g = Graph::new();
        g.add_node(Node::task("a", "anchor"));
        g.set_anchor(&NodeId::from("a"));
        assert!(g.anchor().unwrap().immutable);
    }

    #[test]
    fn leadsto_wire_roundtrips() {
        assert_eq!(RelationType::LeadsTo.as_wire(), "LeadsTo");
        assert!(matches!(RelationType::parse_wire("LeadsTo"), RelationType::LeadsTo));
    }

    #[test]
    fn is_structural_classifies_relations() {
        assert!(RelationType::LeadsTo.is_structural());
        assert!(RelationType::DependsOn.is_structural());
        assert!(RelationType::Contains.is_structural());
        assert!(!RelationType::RevealedBy.is_structural());
        assert!(!RelationType::InvalidatedBy.is_structural());
    }
}
