//! Graph validation — structural consistency checks.
//!
//! Validation in Phase 1 is purely deterministic (no model needed). It is
//! the first half of the verification step in design doc §6.2 — catching
//! defects that can be detected without going back to source.
//!
//! The output is a structured `Vec<Inconsistency>` so that the future
//! local-repair flow (design doc §6.2, project principle #3) can route
//! each issue to a scoped fix.

use super::{Graph, NodeId, RelationType};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Inconsistency {
    /// An edge endpoint refers to a non-existent node. Should not happen
    /// when edges are added via [`Graph::add_edge`], but can appear after
    /// JSON deserialization or careless mutation.
    DanglingEdge {
        edge_idx: usize,
        missing_endpoint: NodeId,
    },
    /// Node has neither incoming nor outgoing edges. May be intentional —
    /// the caller decides; we just report.
    OrphanNode { node: NodeId },
    /// A cycle exists in a relation that should be acyclic (e.g. `DependsOn`).
    Cycle {
        cycle: Vec<NodeId>,
        relation: RelationType,
    },
    /// Two edges with identical (source, target, relation).
    DuplicateEdge { first_idx: usize, second_idx: usize },
    /// Edge confidence outside [0,1] — only reachable through deserialization
    /// of hand-edited JSON; constructor clamps.
    InvalidConfidence { edge_idx: usize, value: f64 },
}

impl Graph {
    /// Run all structural checks and return the issues found.
    ///
    /// Returns an empty `Vec` if the graph is structurally consistent.
    /// Note that "consistent" here is weaker than "verified" (which also
    /// requires sampled-against-source verification by the model in Phase 2).
    pub fn find_inconsistencies(&self) -> Vec<Inconsistency> {
        let mut issues = Vec::new();
        self.check_dangling_edges(&mut issues);
        self.check_orphans(&mut issues);
        self.check_duplicate_edges(&mut issues);
        self.check_invalid_confidence(&mut issues);
        // Acyclic-required relations
        for rel in ACYCLIC_RELATIONS {
            if let Some(cycle) = self.find_cycle_in_relation(rel.clone()) {
                issues.push(Inconsistency::Cycle {
                    cycle,
                    relation: rel.clone(),
                });
            }
        }
        issues
    }

    fn check_dangling_edges(&self, issues: &mut Vec<Inconsistency>) {
        for (idx, edge) in self.edges.iter().enumerate() {
            if !self.contains_node(&edge.source) {
                issues.push(Inconsistency::DanglingEdge {
                    edge_idx: idx,
                    missing_endpoint: edge.source.clone(),
                });
            }
            if !self.contains_node(&edge.target) {
                issues.push(Inconsistency::DanglingEdge {
                    edge_idx: idx,
                    missing_endpoint: edge.target.clone(),
                });
            }
        }
    }

    fn check_orphans(&self, issues: &mut Vec<Inconsistency>) {
        for id in self.nodes.keys() {
            if self.outgoing(id).next().is_none() && self.incoming(id).next().is_none() {
                issues.push(Inconsistency::OrphanNode { node: id.clone() });
            }
        }
    }

    fn check_duplicate_edges(&self, issues: &mut Vec<Inconsistency>) {
        let mut seen: HashMap<(NodeId, NodeId, RelationType), usize> = HashMap::new();
        for (idx, edge) in self.edges.iter().enumerate() {
            let key = (edge.source.clone(), edge.target.clone(), edge.relation.clone());
            if let Some(&prev) = seen.get(&key) {
                issues.push(Inconsistency::DuplicateEdge {
                    first_idx: prev,
                    second_idx: idx,
                });
            } else {
                seen.insert(key, idx);
            }
        }
    }

    fn check_invalid_confidence(&self, issues: &mut Vec<Inconsistency>) {
        for (idx, edge) in self.edges.iter().enumerate() {
            if !(0.0..=1.0).contains(&edge.confidence) || edge.confidence.is_nan() {
                issues.push(Inconsistency::InvalidConfidence {
                    edge_idx: idx,
                    value: edge.confidence,
                });
            }
        }
    }

    /// DFS-based cycle detection restricted to a single relation type.
    /// Returns the cycle (as a vector of node ids forming the back-edge
    /// loop) on first detection, or `None` if acyclic.
    pub fn find_cycle_in_relation(&self, rel: RelationType) -> Option<Vec<NodeId>> {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Unseen,
            InProgress,
            Done,
        }
        let mut state: HashMap<NodeId, Mark> = self
            .nodes
            .keys()
            .map(|k| (k.clone(), Mark::Unseen))
            .collect();
        let mut stack: Vec<NodeId> = Vec::new();
        let mut path: Vec<NodeId> = Vec::new();

        for start in self.nodes.keys() {
            if state[start] != Mark::Unseen {
                continue;
            }
            stack.clear();
            path.clear();
            stack.push(start.clone());
            while let Some(top) = stack.last().cloned() {
                match state[&top] {
                    Mark::Unseen => {
                        *state.get_mut(&top).unwrap() = Mark::InProgress;
                        path.push(top.clone());
                    }
                    Mark::InProgress => { /* still expanding */ }
                    Mark::Done => {
                        stack.pop();
                        continue;
                    }
                }

                let mut advanced = false;
                let neighbors: Vec<NodeId> = self
                    .outgoing(&top)
                    .filter(|e| e.relation == rel)
                    .map(|e| e.target.clone())
                    .collect();

                for n in neighbors {
                    match state.get(&n).copied().unwrap_or(Mark::Unseen) {
                        Mark::Unseen => {
                            stack.push(n);
                            advanced = true;
                            break;
                        }
                        Mark::InProgress => {
                            // Found a back-edge — extract cycle from `path`.
                            if let Some(pos) = path.iter().position(|x| x == &n) {
                                let mut cycle: Vec<NodeId> = path[pos..].to_vec();
                                cycle.push(n);
                                return Some(cycle);
                            }
                        }
                        Mark::Done => {}
                    }
                }

                if !advanced {
                    *state.get_mut(&top).unwrap() = Mark::Done;
                    if path.last() == Some(&top) {
                        path.pop();
                    }
                    stack.pop();
                }
            }
        }
        None
    }
}

const ACYCLIC_RELATIONS: &[RelationType] = &[RelationType::DependsOn, RelationType::Contains];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, Node};

    #[test]
    fn clean_graph_has_no_issues_except_isolated_nodes() {
        let mut g = Graph::new();
        g.add_node(Node::file("a", "A"));
        g.add_node(Node::file("b", "B"));
        g.add_edge(Edge::new("a", "b", RelationType::Imports, 1.0, ""))
            .unwrap();
        let issues = g.find_inconsistencies();
        // No orphans in this graph
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
    }

    #[test]
    fn detects_orphan_node() {
        let mut g = Graph::new();
        g.add_node(Node::file("a", "A"));
        let issues = g.find_inconsistencies();
        assert!(matches!(
            issues.as_slice(),
            [Inconsistency::OrphanNode { .. }]
        ));
    }

    #[test]
    fn detects_duplicate_edge() {
        let mut g = Graph::new();
        g.add_node(Node::file("a", "A"));
        g.add_node(Node::file("b", "B"));
        g.add_edge(Edge::new("a", "b", RelationType::Imports, 1.0, ""))
            .unwrap();
        g.add_edge(Edge::new("a", "b", RelationType::Imports, 0.5, ""))
            .unwrap();
        let issues = g.find_inconsistencies();
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, Inconsistency::DuplicateEdge { .. })),
            "expected DuplicateEdge in {issues:?}"
        );
    }

    #[test]
    fn detects_dangling_edge_after_manual_node_removal() {
        let mut g = Graph::new();
        g.add_node(Node::file("a", "A"));
        g.add_node(Node::file("b", "B"));
        g.add_edge(Edge::new("a", "b", RelationType::Imports, 1.0, ""))
            .unwrap();
        // Bypass the safe API by direct nodes-map manipulation
        g.nodes.remove(&NodeId::from("b"));
        let issues = g.find_inconsistencies();
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, Inconsistency::DanglingEdge { .. })),
            "expected DanglingEdge in {issues:?}"
        );
    }

    #[test]
    fn detects_cycle_in_depends_on() {
        let mut g = Graph::new();
        g.add_node(Node::task("t1", "T1"));
        g.add_node(Node::task("t2", "T2"));
        g.add_node(Node::task("t3", "T3"));
        g.add_edge(Edge::new("t1", "t2", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        g.add_edge(Edge::new("t2", "t3", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        g.add_edge(Edge::new("t3", "t1", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        let issues = g.find_inconsistencies();
        let has_cycle = issues
            .iter()
            .any(|i| matches!(i, Inconsistency::Cycle { relation, .. } if relation == &RelationType::DependsOn));
        assert!(has_cycle, "expected Cycle in {issues:?}");
    }

    #[test]
    fn leadsto_cycle_is_allowed() {
        let mut g = Graph::new();
        g.add_node(Node::task("x", "X"));
        g.add_node(Node::task("y", "Y"));
        g.add_edge(Edge::new("x", "y", RelationType::LeadsTo, 1.0, ""))
            .unwrap();
        g.add_edge(Edge::new("y", "x", RelationType::LeadsTo, 1.0, ""))
            .unwrap();
        // LeadsTo cycles must NOT be reported as inconsistencies.
        let issues = g.find_inconsistencies();
        assert!(
            !issues.iter().any(|i| matches!(i, Inconsistency::Cycle { relation, .. } if *relation == RelationType::LeadsTo)),
            "LeadsTo cycle should not be reported, but got: {issues:?}"
        );
    }

    #[test]
    fn detects_cycle_in_contains() {
        let mut g = Graph::new();
        g.add_node(Node::task("a", "A"));
        g.add_node(Node::task("b", "B"));
        g.add_edge(Edge::new("a", "b", RelationType::Contains, 1.0, ""))
            .unwrap();
        g.add_edge(Edge::new("b", "a", RelationType::Contains, 1.0, ""))
            .unwrap();
        let issues = g.find_inconsistencies();
        assert!(
            issues.iter().any(|i| matches!(i, Inconsistency::Cycle { relation, .. } if *relation == RelationType::Contains)),
            "expected Contains Cycle in {issues:?}"
        );
    }

    #[test]
    fn imports_cycle_is_not_reported_by_default() {
        // We only mark `DependsOn` as acyclic-required. `Imports` may form
        // cycles legitimately (modules importing each other) — caller can
        // run `find_cycle_in_relation` explicitly if they want to check.
        let mut g = Graph::new();
        g.add_node(Node::file("a", "A"));
        g.add_node(Node::file("b", "B"));
        g.add_edge(Edge::new("a", "b", RelationType::Imports, 1.0, ""))
            .unwrap();
        g.add_edge(Edge::new("b", "a", RelationType::Imports, 1.0, ""))
            .unwrap();
        let issues = g.find_inconsistencies();
        assert!(!issues.iter().any(|i| matches!(i, Inconsistency::Cycle { .. })));
    }
}
