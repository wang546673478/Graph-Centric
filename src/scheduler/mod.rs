//! DAG scheduler — turns a task graph into wave-aligned batches.
//!
//! The scheduler operates on the *same* [`Graph`] type as the world graph;
//! it just expects nodes to be `NodeKind::Task` and dependencies to be
//! encoded as `RelationType::DependsOn` edges.
//!
//! ## Direction convention
//!
//! An edge `A —DependsOn→ B` means **A needs B to be done first**.
//! `B` is the prerequisite, `A` is the dependent.
//!
//! ## Algorithm
//!
//! Variant of Kahn's algorithm where "in-degree" is the count of *outgoing*
//! `DependsOn` edges (i.e. how many prerequisites a task still has). Tasks
//! with zero remaining prerequisites form the next batch, and completing
//! them decrements the count for their dependents (predecessors via the
//! `DependsOn` relation).
//!
//! Cycles are reported as scheduler errors rather than silently truncated.

use crate::error::{HarnessError, Result};
use crate::graph::{Graph, NodeId, NodeKind, RelationType};
use std::collections::{HashMap, HashSet};

/// Wave-aligned execution plan. `batches[i]` is the set of task ids ready
/// to run in parallel after `batches[0..i]` have completed.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub batches: Vec<Vec<NodeId>>,
}

impl Schedule {
    pub fn task_count(&self) -> usize {
        self.batches.iter().map(Vec::len).sum()
    }

    pub fn depth(&self) -> usize {
        self.batches.len()
    }

    /// Flattened topological order. Useful when concurrency is forbidden
    /// (e.g. a sequential dry run).
    pub fn linear(&self) -> Vec<NodeId> {
        self.batches.iter().flatten().cloned().collect()
    }
}

#[derive(Debug, Clone)]
pub struct DagScheduler {
    /// If `Some`, only nodes of this kind are considered. Default `Some(Task)`.
    pub restrict_kind: Option<NodeKind>,
    /// Relation that encodes "must come first". Default `DependsOn`.
    pub dep_relation: RelationType,
    /// Optional cap on batch size — useful to limit concurrent sub-agents.
    /// `None` means no cap.
    pub max_batch_size: Option<usize>,
}

impl Default for DagScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl DagScheduler {
    pub fn new() -> Self {
        Self {
            restrict_kind: Some(NodeKind::Task),
            dep_relation: RelationType::DependsOn,
            max_batch_size: None,
        }
    }

    pub fn with_max_batch_size(mut self, n: usize) -> Self {
        self.max_batch_size = Some(n);
        self
    }

    pub fn without_kind_restriction(mut self) -> Self {
        self.restrict_kind = None;
        self
    }

    pub fn plan(&self, graph: &Graph) -> Result<Schedule> {
        // Collect the subset of nodes the scheduler cares about.
        let nodes: HashSet<NodeId> = graph
            .iter_nodes()
            .filter(|n| match &self.restrict_kind {
                Some(k) => &n.kind == k,
                None => true,
            })
            .map(|n| n.id.clone())
            .collect();

        if nodes.is_empty() {
            return Ok(Schedule { batches: vec![] });
        }

        // dep_count[t] = remaining prerequisites for task `t`.
        let mut dep_count: HashMap<NodeId, usize> = HashMap::with_capacity(nodes.len());
        for id in &nodes {
            let count = graph
                .outgoing(id)
                .filter(|e| e.relation == self.dep_relation && nodes.contains(&e.target))
                .count();
            dep_count.insert(id.clone(), count);
        }

        let mut batches: Vec<Vec<NodeId>> = Vec::new();
        while !dep_count.is_empty() {
            // Pick all currently-ready tasks. Sort by priority
            // (descending) so critical-path tasks fire first within
            // a wave, then by NodeId for determinism. v2 spec §5.4.
            let mut ready: Vec<NodeId> = dep_count
                .iter()
                .filter(|&(_, &c)| c == 0)
                .map(|(k, _)| k.clone())
                .collect();
            ready.sort_by(|a, b| {
                let pa = graph.get_node(a).map(|n| -n.priority()).unwrap_or(0);
                let pb = graph.get_node(b).map(|n| -n.priority()).unwrap_or(0);
                pa.cmp(&pb).then_with(|| a.as_str().cmp(b.as_str()))
            });

            if ready.is_empty() {
                return Err(HarnessError::scheduler(format!(
                    "cycle in task DAG via relation {:?}: {} tasks remain",
                    self.dep_relation,
                    dep_count.len()
                )));
            }

            // Optional batch-size cap. Splitting into chunks doesn't change
            // correctness — anything that *was* ready stays ready next wave.
            let chunks: Vec<Vec<NodeId>> = match self.max_batch_size {
                Some(cap) if cap > 0 => ready.chunks(cap).map(|c| c.to_vec()).collect(),
                _ => vec![ready],
            };

            for chunk in chunks {
                for done in &chunk {
                    dep_count.remove(done);
                }
                // Decrement counts for tasks that depended on the completed ones.
                for done in &chunk {
                    let preds: Vec<NodeId> = graph
                        .incoming(done)
                        .filter(|e| e.relation == self.dep_relation)
                        .map(|e| e.source.clone())
                        .collect();
                    for p in preds {
                        if let Some(c) = dep_count.get_mut(&p) {
                            *c = c.saturating_sub(1);
                        }
                    }
                }
                batches.push(chunk);
            }
        }

        Ok(Schedule { batches })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, Node, RelationType};

    fn diamond_task_graph() -> Graph {
        // t1, t2 → t3 → t4, t5 → t6  (per design doc §7.3 diagram)
        // Edge convention: dependent —DependsOn→ prerequisite.
        let mut g = Graph::new();
        for id in ["t1", "t2", "t3", "t4", "t5", "t6"] {
            g.add_node(Node::task(id, id));
        }
        // t3 depends on t1, t2
        g.add_edge(Edge::new("t3", "t1", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        g.add_edge(Edge::new("t3", "t2", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        // t4, t5 depend on t3
        g.add_edge(Edge::new("t4", "t3", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        g.add_edge(Edge::new("t5", "t3", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        // t6 depends on t4, t5
        g.add_edge(Edge::new("t6", "t4", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        g.add_edge(Edge::new("t6", "t5", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        g
    }

    #[test]
    fn diamond_produces_four_batches() {
        let g = diamond_task_graph();
        let s = DagScheduler::new().plan(&g).unwrap();
        assert_eq!(s.depth(), 4);
        assert_eq!(s.task_count(), 6);
        // Batch 0: {t1, t2}
        assert_eq!(s.batches[0], vec![NodeId::from("t1"), NodeId::from("t2")]);
        // Batch 1: {t3}
        assert_eq!(s.batches[1], vec![NodeId::from("t3")]);
        // Batch 2: {t4, t5}
        assert_eq!(s.batches[2], vec![NodeId::from("t4"), NodeId::from("t5")]);
        // Batch 3: {t6}
        assert_eq!(s.batches[3], vec![NodeId::from("t6")]);
    }

    #[test]
    fn cycle_returns_error() {
        let mut g = Graph::new();
        for id in ["a", "b"] {
            g.add_node(Node::task(id, id));
        }
        g.add_edge(Edge::new("a", "b", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        g.add_edge(Edge::new("b", "a", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        let err = DagScheduler::new().plan(&g).unwrap_err();
        assert!(format!("{err}").contains("cycle"));
    }

    #[test]
    fn max_batch_size_splits_wide_waves() {
        let mut g = Graph::new();
        for id in ["a", "b", "c", "d", "e"] {
            g.add_node(Node::task(id, id));
        }
        // No dependencies — all ready in one wave by default
        let s = DagScheduler::new()
            .with_max_batch_size(2)
            .plan(&g)
            .unwrap();
        // 5 tasks split into [2, 2, 1]
        assert_eq!(s.depth(), 3);
        assert_eq!(s.batches[0].len(), 2);
        assert_eq!(s.batches[1].len(), 2);
        assert_eq!(s.batches[2].len(), 1);
    }

    #[test]
    fn empty_graph_returns_empty_schedule() {
        let g = Graph::new();
        let s = DagScheduler::new().plan(&g).unwrap();
        assert!(s.batches.is_empty());
    }

    #[test]
    fn non_task_nodes_are_ignored_by_default() {
        let mut g = Graph::new();
        g.add_node(Node::file("f.rs", "F"));
        g.add_node(Node::task("t1", "T1"));
        let s = DagScheduler::new().plan(&g).unwrap();
        assert_eq!(s.task_count(), 1);
        assert_eq!(s.batches[0], vec![NodeId::from("t1")]);
    }
}
