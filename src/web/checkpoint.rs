//! Per-run checkpoint store for conversation branching and history replay.

use super::persistence::RunPersistence;
use crate::graph::Graph;
use crate::model::Message;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub index: usize,
    pub round: usize,
    pub phase: CheckpointPhase,
    pub graph_snapshot: Graph,
    pub transcript: Vec<Message>,
    /// Links from this run's complex nodes to forked sub-runs.
    /// Populated by `RunPersistence::append_sub_run_link`; consumed by the
    /// API and frontend to render the drill-down graph.
    /// `#[serde(default)]` keeps backward compatibility with older
    /// checkpoint files written before Task 8.
    #[serde(default)]
    pub sub_run_links: Vec<SubRunLink>,
}

/// Link from a complex node in a parent graph to its forked sub-run.
/// Persisted inside the latest parent checkpoint so the parent loop
/// can poll the child loop's status and update the node's
/// `expanded`/`sub_run_status` metadata when the child finishes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubRunLink {
    pub node_id: crate::graph::NodeId,
    pub sub_run_id: String,
    pub sub_status: String, // "running" | "done" | "error"
    pub created_at: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum CheckpointPhase {
    Graph,
    Task,
    Review,
}

impl std::fmt::Display for CheckpointPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Graph => write!(f, "graph"),
            Self::Task => write!(f, "task"),
            Self::Review => write!(f, "review"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CheckpointStore {
    checkpoints: Vec<Checkpoint>,
    /// checkpoint_index → [child_run_ids]
    branches: HashMap<usize, Vec<String>>,
    /// Optional persistence — flushes to disk on push/create_branch.
    pub persistence: Option<RunPersistence>,
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckpointMeta {
    pub index: usize,
    pub round: usize,
    pub phase: String,
    pub node_count: usize,
    pub edge_count: usize,
}

impl CheckpointStore {
    pub fn new() -> Self {
        Self {
            checkpoints: Vec::new(),
            branches: HashMap::new(),
            persistence: None,
            run_id: String::new(),
        }
    }

    pub fn with_persistence(mut self, p: &RunPersistence, run_id: &str) -> Self {
        self.persistence = Some(p.clone());
        self.run_id = run_id.to_string();
        self
    }

    pub fn push(
        &mut self,
        round: usize,
        phase: CheckpointPhase,
        graph: &Graph,
        transcript: &[Message],
    ) {
        let cp = Checkpoint {
            index: self.checkpoints.len(),
            round,
            phase,
            graph_snapshot: graph.clone(),
            transcript: transcript.to_vec(),
            sub_run_links: Vec::new(),
        };
        if let Some(ref p) = self.persistence {
            let _ = p.save_checkpoint(&self.run_id, &cp);
        }
        self.checkpoints.push(cp);
    }

    pub fn get(&self, index: usize) -> Option<&Checkpoint> {
        self.checkpoints.get(index)
    }

    pub fn list(&self) -> Vec<CheckpointMeta> {
        self.checkpoints
            .iter()
            .map(|cp| CheckpointMeta {
                index: cp.index,
                round: cp.round,
                phase: cp.phase.to_string(),
                node_count: cp.graph_snapshot.node_count(),
                edge_count: cp.graph_snapshot.edge_count(),
            })
            .collect()
    }

    pub fn create_branch(&mut self, from_index: usize, child_run_id: String) {
        self.branches
            .entry(from_index)
            .or_default()
            .push(child_run_id);
        if let Some(ref p) = self.persistence {
            let _ = p.save_branches(&self.run_id, &self.branches);
        }
    }

    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }
}
