//! Graph traversal — BFS, distance, and local subgraph extraction.
//!
//! These are the deterministic primitives the ContextBuilder relies on to
//! cut a small, structurally-coherent window out of a potentially huge
//! relationship graph. See design doc §8 (three-layer maps + distance-based
//! compression).

use super::{Edge, Graph, Node, NodeId};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

/// Result of a bounded breadth-first traversal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Traversal {
    pub start: Vec<NodeId>,
    pub max_depth: usize,
    pub visited: HashSet<NodeId>,
    /// Graph distance from the nearest start node.
    pub distance: HashMap<NodeId, usize>,
    /// Nodes grouped by distance: `by_depth[d]` are nodes at distance `d`.
    pub by_depth: Vec<Vec<NodeId>>,
}

impl Traversal {
    pub fn nodes_at(&self, depth: usize) -> &[NodeId] {
        self.by_depth.get(depth).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn distance_of(&self, id: &NodeId) -> Option<usize> {
        self.distance.get(id).copied()
    }
}

impl Graph {
    /// Breadth-first traversal from `start` nodes following outgoing edges,
    /// bounded by `max_depth`. Distance 0 is the start set itself.
    pub fn bfs_from(&self, start: &[NodeId], max_depth: usize) -> Traversal {
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut distance: HashMap<NodeId, usize> = HashMap::new();
        let mut by_depth: Vec<Vec<NodeId>> = vec![Vec::new(); max_depth + 1];
        let mut queue: VecDeque<(NodeId, usize)> = VecDeque::new();

        for s in start {
            if self.contains_node(s) && visited.insert(s.clone()) {
                distance.insert(s.clone(), 0);
                by_depth[0].push(s.clone());
                queue.push_back((s.clone(), 0));
            }
        }

        while let Some((id, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            let next_depth = depth + 1;
            // Collect neighbor ids first so we don't hold the borrow on self
            // through the loop.
            let neighbors: Vec<NodeId> = self.neighbors(&id).cloned().collect();
            for n in neighbors {
                if visited.insert(n.clone()) {
                    distance.insert(n.clone(), next_depth);
                    by_depth[next_depth].push(n.clone());
                    queue.push_back((n, next_depth));
                }
            }
        }

        Traversal {
            start: start.to_vec(),
            max_depth,
            visited,
            distance,
            by_depth,
        }
    }

    /// Same as [`bfs_from`] but walking incoming edges (reverse direction).
    pub fn bfs_reverse_from(&self, start: &[NodeId], max_depth: usize) -> Traversal {
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut distance: HashMap<NodeId, usize> = HashMap::new();
        let mut by_depth: Vec<Vec<NodeId>> = vec![Vec::new(); max_depth + 1];
        let mut queue: VecDeque<(NodeId, usize)> = VecDeque::new();

        for s in start {
            if self.contains_node(s) && visited.insert(s.clone()) {
                distance.insert(s.clone(), 0);
                by_depth[0].push(s.clone());
                queue.push_back((s.clone(), 0));
            }
        }

        while let Some((id, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            let next_depth = depth + 1;
            let preds: Vec<NodeId> = self.predecessors(&id).cloned().collect();
            for p in preds {
                if visited.insert(p.clone()) {
                    distance.insert(p.clone(), next_depth);
                    by_depth[next_depth].push(p.clone());
                    queue.push_back((p, next_depth));
                }
            }
        }

        Traversal {
            start: start.to_vec(),
            max_depth,
            visited,
            distance,
            by_depth,
        }
    }

    /// Extract the local subgraph around `center` nodes up to `depth`,
    /// in both directions. The subgraph keeps only edges whose endpoints
    /// are both within the visited set.
    ///
    /// This is the key primitive for context construction (Layer 2 of the
    /// three-layer map): pull a structurally coherent slice of the world
    /// graph for a given task.
    pub fn local_subgraph(&self, center: &[NodeId], depth: usize) -> Graph {
        let fwd = self.bfs_from(center, depth);
        let rev = self.bfs_reverse_from(center, depth);

        let mut keep: HashSet<NodeId> = fwd.visited;
        keep.extend(rev.visited);

        let mut sub = Graph::new();
        sub.status = self.status.clone();

        for id in &keep {
            if let Some(node) = self.get_node(id) {
                sub.insert_node_raw(node.clone());
            }
        }
        for edge in &self.edges {
            if keep.contains(&edge.source) && keep.contains(&edge.target) {
                sub.insert_edge_raw(edge.clone());
            }
        }
        sub.rebuild_indices();
        sub
    }

    /// Transitive set of nodes that depend on `id` (via any incoming
    /// edge). Useful when something changes and we need to know what
    /// might be affected.
    pub fn dependents_of(&self, id: &NodeId) -> Vec<NodeId> {
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut queue: VecDeque<NodeId> = VecDeque::new();
        queue.push_back(id.clone());
        visited.insert(id.clone());
        while let Some(cur) = queue.pop_front() {
            let preds: Vec<NodeId> = self.predecessors(&cur).cloned().collect();
            for p in preds {
                if visited.insert(p.clone()) {
                    queue.push_back(p);
                }
            }
        }
        visited.remove(id);
        visited.into_iter().collect()
    }

    /// Shortest distance between two nodes following outgoing edges, or
    /// `None` if unreachable.
    pub fn distance_to(&self, from: &NodeId, to: &NodeId) -> Option<usize> {
        if !self.contains_node(from) || !self.contains_node(to) {
            return None;
        }
        if from == to {
            return Some(0);
        }
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut queue: VecDeque<(NodeId, usize)> = VecDeque::new();
        queue.push_back((from.clone(), 0));
        visited.insert(from.clone());
        while let Some((cur, d)) = queue.pop_front() {
            let neighbors: Vec<NodeId> = self.neighbors(&cur).cloned().collect();
            for n in neighbors {
                if &n == to {
                    return Some(d + 1);
                }
                if visited.insert(n.clone()) {
                    queue.push_back((n, d + 1));
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Helpers re-exported for ContextBuilder etc.
// ---------------------------------------------------------------------------

/// Collect all edges of a subgraph view: edges where both endpoints are in `nodes`.
pub fn edges_within<'a>(
    graph: &'a Graph,
    nodes: &HashSet<NodeId>,
) -> impl Iterator<Item = (usize, &'a Edge)> + 'a {
    let nodes_owned: HashSet<NodeId> = nodes.clone();
    graph
        .iter_edges()
        .enumerate()
        .filter(move |(_, e)| nodes_owned.contains(&e.source) && nodes_owned.contains(&e.target))
}

/// Collect all nodes reachable in the union of forward and reverse BFS.
pub fn neighborhood(graph: &Graph, center: &[NodeId], depth: usize) -> HashSet<NodeId> {
    let mut s = graph.bfs_from(center, depth).visited;
    s.extend(graph.bfs_reverse_from(center, depth).visited);
    s
}

// Avoid an "unused" warning for `Node` when this file is compiled in isolation
// during incremental builds — it's referenced in doctest paths only.
#[allow(dead_code)]
fn _phantom_node(_n: &Node) {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::RelationType;

    fn chain_graph() -> Graph {
        // a → b → c → d
        let mut g = Graph::new();
        g.add_node(Node::file("a", "A"));
        g.add_node(Node::file("b", "B"));
        g.add_node(Node::file("c", "C"));
        g.add_node(Node::file("d", "D"));
        g.add_edge(Edge::new("a", "b", RelationType::Imports, 1.0, ""))
            .unwrap();
        g.add_edge(Edge::new("b", "c", RelationType::Imports, 1.0, ""))
            .unwrap();
        g.add_edge(Edge::new("c", "d", RelationType::Imports, 1.0, ""))
            .unwrap();
        g
    }

    #[test]
    fn bfs_respects_depth_limit() {
        let g = chain_graph();
        let t = g.bfs_from(&[NodeId::from("a")], 2);
        assert!(t.visited.contains(&NodeId::from("a")));
        assert!(t.visited.contains(&NodeId::from("b")));
        assert!(t.visited.contains(&NodeId::from("c")));
        assert!(!t.visited.contains(&NodeId::from("d")));
        assert_eq!(t.distance_of(&NodeId::from("a")), Some(0));
        assert_eq!(t.distance_of(&NodeId::from("c")), Some(2));
    }

    #[test]
    fn reverse_bfs_walks_incoming() {
        let g = chain_graph();
        let t = g.bfs_reverse_from(&[NodeId::from("d")], 3);
        assert!(t.visited.contains(&NodeId::from("a")));
        assert_eq!(t.distance_of(&NodeId::from("a")), Some(3));
    }

    #[test]
    fn local_subgraph_includes_both_directions() {
        let g = chain_graph();
        let sub = g.local_subgraph(&[NodeId::from("b")], 1);
        // Forward 1: b → c. Reverse 1: a → b. All three present.
        assert!(sub.contains_node(&NodeId::from("a")));
        assert!(sub.contains_node(&NodeId::from("b")));
        assert!(sub.contains_node(&NodeId::from("c")));
        assert!(!sub.contains_node(&NodeId::from("d")));
        // Edges: a→b and b→c
        assert_eq!(sub.edge_count(), 2);
    }

    #[test]
    fn dependents_collects_transitive_predecessors() {
        let g = chain_graph();
        let deps = g.dependents_of(&NodeId::from("d"));
        // a, b, c all transitively depend on d
        assert_eq!(deps.len(), 3);
    }

    #[test]
    fn distance_to_returns_shortest_path_length() {
        let g = chain_graph();
        assert_eq!(g.distance_to(&NodeId::from("a"), &NodeId::from("d")), Some(3));
        assert_eq!(g.distance_to(&NodeId::from("a"), &NodeId::from("a")), Some(0));
        assert_eq!(g.distance_to(&NodeId::from("d"), &NodeId::from("a")), None);
    }
}
