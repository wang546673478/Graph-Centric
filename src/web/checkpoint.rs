//! Per-run checkpoint store for conversation branching and history replay.

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        Self::default()
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
        };
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
    }

    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }
}
