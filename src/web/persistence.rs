//! Run and checkpoint persistence — JSON files on disk.
//!
//! Layout under `data/runs/<run_id>/`:
//!   run.json          — RunMetadata
//!   checkpoints/      — 0000.json, 0001.json, ...
//!   branches.json     — HashMap<usize, Vec<String>>
//!
//! This module is pure file I/O; callers are responsible for thread safety.

use super::checkpoint::{Checkpoint, CheckpointMeta};
use super::run_session::{RunMetadata, RunStatus};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

const CHECKPOINT_FMT: usize = 4; // zero-padded width for checkpoint filenames

#[derive(Debug, Clone)]
pub struct RunPersistence {
    pub data_dir: PathBuf,
}

impl RunPersistence {
    pub fn new(project_root: &Path) -> Self {
        Self { data_dir: project_root.join("data").join("runs") }
    }

    /// Construct a persistence rooted directly at `data_dir`. Used by tests
    /// (and a few internal helpers) that want to bypass the
    /// `<project>/data/runs/` layout. Production paths stick with
    /// `RunPersistence::new`.
    pub fn with_data_dir(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    fn run_dir(&self, run_id: &str) -> PathBuf {
        self.data_dir.join(run_id)
    }

    fn checkpoints_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("checkpoints")
    }

    fn checkpoint_path(&self, run_id: &str, index: usize) -> PathBuf {
        self.checkpoints_dir(run_id).join(format!("{index:0CHECKPOINT_FMT$}.json"))
    }

    fn branches_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("branches.json")
    }

    /// Ensure the directory structure for `run_id` exists.
    pub fn ensure_dirs(&self, run_id: &str) -> std::io::Result<()> {
        let d = self.checkpoints_dir(run_id);
        std::fs::create_dir_all(&d)
    }

    // ---- Run metadata ----

    pub fn save_run_meta(&self, meta: &RunMetadata) -> std::io::Result<()> {
        let dir = self.run_dir(&meta.id);
        std::fs::create_dir_all(&dir)?;
        let json = serde_json::to_string_pretty(meta)?;
        std::fs::write(dir.join("run.json"), json)
    }

    /// Scan `data/runs/` and return all persisted run metadata.
    pub fn load_all_runs(&self) -> std::io::Result<Vec<RunMetadata>> {
        let mut out = Vec::new();
        if !self.data_dir.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&self.data_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let run_json = entry.path().join("run.json");
                if run_json.exists() {
                    match serde_json::from_str::<RunMetadata>(&std::fs::read_to_string(&run_json)?) {
                        Ok(meta) => out.push(meta),
                        Err(e) => warn!(path = %run_json.display(), error = %e, "persistence: corrupt run.json, skipping"),
                    }
                }
            }
        }
        Ok(out)
    }

    /// Remove a run's directory from disk.
    pub fn delete_run(&self, run_id: &str) -> std::io::Result<()> {
        let dir = self.run_dir(run_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
        } else {
            Ok(())
        }
    }

    // ---- Checkpoints ----

    pub fn save_checkpoint(&self, run_id: &str, cp: &Checkpoint) -> std::io::Result<()> {
        self.ensure_dirs(run_id)?;
        let json = serde_json::to_string_pretty(cp)?;
        std::fs::write(self.checkpoint_path(run_id, cp.index), json)
    }

    pub fn load_checkpoint(&self, run_id: &str, index: usize) -> std::io::Result<Option<Checkpoint>> {
        let p = self.checkpoint_path(run_id, index);
        if !p.exists() { return Ok(None); }
        let cp: Checkpoint = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
        Ok(Some(cp))
    }

    /// Returns lightweight metadata for all stored checkpoints.
    pub fn load_all_checkpoint_metas(&self, run_id: &str) -> std::io::Result<Vec<CheckpointMeta>> {
        let dir = self.checkpoints_dir(run_id);
        if !dir.exists() { return Ok(Vec::new()); }
        let mut metas = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let p = entry.path();
            if p.extension().map_or(false, |e| e == "json") {
                match serde_json::from_str::<Checkpoint>(&std::fs::read_to_string(&p)?) {
                    Ok(cp) => metas.push(CheckpointMeta {
                        index: cp.index,
                        round: cp.round,
                        phase: cp.phase.to_string(),
                        node_count: cp.graph_snapshot.node_count(),
                        edge_count: cp.graph_snapshot.edge_count(),
                    }),
                    Err(e) => warn!(path = %p.display(), error = %e, "persistence: corrupt checkpoint, skipping"),
                }
            }
        }
        metas.sort_by_key(|m| m.index);
        Ok(metas)
    }

    // ---- Branches ----

    pub fn save_branches(&self, run_id: &str, branches: &HashMap<usize, Vec<String>>) -> std::io::Result<()> {
        self.ensure_dirs(run_id)?;
        let json = serde_json::to_string_pretty(branches)?;
        std::fs::write(self.branches_path(run_id), json)
    }

    pub fn load_branches(&self, run_id: &str) -> std::io::Result<HashMap<usize, Vec<String>>> {
        let p = self.branches_path(run_id);
        if !p.exists() { return Ok(HashMap::new()); }
        Ok(serde_json::from_str(&std::fs::read_to_string(&p)?)?)
    }

    // ---- List run IDs ----

    pub fn list_run_ids(&self) -> std::io::Result<Vec<String>> {
        let mut ids = Vec::new();
        if !self.data_dir.exists() { return Ok(ids); }
        for entry in std::fs::read_dir(&self.data_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                ids.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        Ok(ids)
    }

    // ---- Sub-runs (Task 6 stub — Task 8 will replace with real impl) ----

    /// Create the directory for a sub-run under `data_dir/<parent>/sub_runs/<sub>/`.
    /// TODO(task-8): replace with the real layout that uses `<project>/data/runs/<parent>/sub_runs/<sub>/`.
    pub fn create_sub_run_dir(&self, parent: &str, sub: &str) -> std::io::Result<PathBuf> {
        let dir = self.data_dir.join(parent).join("sub_runs").join(sub);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Return a fresh [`RunPersistence`] rooted at the sub-run dir. Used by
    /// `GraphLoop::fork_sub_graph_for` so the child loop writes its own
    /// `run.json` to the sub-run directory.
    /// TODO(task-8): replace with the real layout.
    pub fn clone_for_sub_run(&self, parent: &str, sub: &str) -> Self {
        let dir = self.data_dir.join(parent).join("sub_runs").join(sub);
        Self::with_data_dir(dir)
    }

    /// Append a [`SubRunLink`] to the parent's checkpoint index. Task 6
    /// stub: logs a TODO line and writes nothing on disk. Task 8 will
    /// implement the real append (atomic write to
    /// `<parent>/sub_runs/links.json`).
    pub fn append_sub_run_link(&self, parent: &str, link: &SubRunLink) {
        tracing::warn!(
            parent = %parent,
            node = %link.node_id.as_str(),
            sub_run_id = %link.sub_run_id,
            "TODO(task-8): persist SubRunLink to disk (in-memory only for now)"
        );
    }

    /// Return the path to a sub-run's `run.json` (the child's persisted
    /// status). The path matches the directory produced by
    /// [`Self::create_sub_run_dir`] / [`Self::clone_for_sub_run`]:
    /// `data_dir/<parent>/sub_runs/<sub>/run.json`. Used by
    /// `GraphLoop::poll_sub_run_status` (Task 7) to read the child loop's
    /// status. Note: this only constructs the path; callers should treat a
    /// `NotFound` from `read_to_string` as "sub-run still running" rather
    /// than an error.
    pub fn sub_run_run_json(&self, parent: &str, sub: &str) -> PathBuf {
        self.data_dir.join(parent).join("sub_runs").join(sub).join("run.json")
    }
}

/// Link from a complex node in the parent graph to its forked sub-run.
/// Captured by `GraphLoop::fork_sub_graph_for` so the parent loop can poll
/// the child loop's status and update the node's `expanded`/`sub_run_status`
/// metadata when the child finishes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubRunLink {
    pub node_id: crate::graph::NodeId,
    pub sub_run_id: String,
    pub sub_status: String,
    pub created_at: u64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::checkpoint::CheckpointPhase;
    use crate::graph::{Graph, Node};
    use crate::model::Message;
    use super::super::run_session::{RunMetadata, RunStatus};

    fn make_meta(id: &str) -> RunMetadata {
        RunMetadata {
            id: id.into(), task: "test".into(),
            status: RunStatus::Done,
            duration_ms: 5000, captured_skill: None,
            tokens_used: 0,
        }
    }

    #[test]
    fn round_trip_run_meta() {
        let dir = tempfile::tempdir().unwrap();
        let p = RunPersistence::new(dir.path());
        let meta = make_meta("r1");
        p.save_run_meta(&meta).unwrap();
        let loaded = p.load_all_runs().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "r1");
        assert_eq!(loaded[0].task, "test");
    }

    #[test]
    fn round_trip_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let p = RunPersistence::new(dir.path());
        let mut g = Graph::new();
        g.add_node(Node::task("a", "anchor"));
        let cp = Checkpoint {
            index: 3, round: 7, phase: CheckpointPhase::Task,
            graph_snapshot: g,
            transcript: vec![Message::user("hi".to_string())],
        };
        p.ensure_dirs("r1").unwrap();
        p.save_checkpoint("r1", &cp).unwrap();
        let loaded = p.load_checkpoint("r1", 3).unwrap().unwrap();
        assert_eq!(loaded.index, 3);
        assert_eq!(loaded.round, 7);
        assert_eq!(loaded.transcript.len(), 1);
    }

    #[test]
    fn round_trip_branches() {
        let dir = tempfile::tempdir().unwrap();
        let p = RunPersistence::new(dir.path());
        let mut b = HashMap::new();
        b.insert(0, vec!["child1".into()]);
        p.ensure_dirs("r1").unwrap();
        p.save_branches("r1", &b).unwrap();
        let loaded = p.load_branches("r1").unwrap();
        assert_eq!(loaded.get(&0).unwrap(), &vec!["child1"]);
    }

    #[test]
    fn load_non_existent_run_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = RunPersistence::new(dir.path());
        assert!(p.load_all_runs().unwrap().is_empty());
        assert!(p.load_checkpoint("dne", 0).unwrap().is_none());
        assert!(p.load_branches("dne").unwrap().is_empty());
    }
}
