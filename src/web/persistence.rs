//! Run and checkpoint persistence — JSON files on disk.
//!
//! Layout under `data/runs/<run_id>/`:
//!   run.json          — RunMetadata
//!   checkpoints/      — 0000.json, 0001.json, ...
//!   branches.json     — HashMap<usize, Vec<String>>
//!
//! This module is pure file I/O; callers are responsible for thread safety.

use super::checkpoint::{Checkpoint, CheckpointMeta, SubRunLink};
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

    // ---- Sub-runs (Task 6 layout; Task 8 wires the real persistence) ----

    /// Create the directory for a sub-run under `data_dir/<parent>/sub_runs/<sub>/`.
    /// Returns the directory path on success; callers can ignore IO errors
    /// (the directory may already exist on a re-fork).
    pub fn create_sub_run_dir(&self, parent_run_id: &str, sub_run_id: &str) -> std::io::Result<PathBuf> {
        let dir = self.data_dir.join(parent_run_id).join("sub_runs").join(sub_run_id);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Return a fresh [`RunPersistence`] rooted at the sub-run dir. Used by
    /// `GraphLoop::fork_sub_graph_for` so the child loop writes its own
    /// `run.json` to the sub-run directory.
    pub fn clone_for_sub_run(&self, parent: &str, sub: &str) -> Self {
        let dir = self.data_dir.join(parent).join("sub_runs").join(sub);
        Self::with_data_dir(dir)
    }

    /// Append a [`SubRunLink`] to the latest parent checkpoint's
    /// `sub_run_links` list. Reads the most recent `checkpoints/*.json`
    /// (highest filename), pushes the link, and writes it back. No-op if
    /// the parent has no checkpoint yet (warning logged).
    pub fn append_sub_run_link(&self, parent_run_id: &str, link: &SubRunLink) {
        let ckpt_dir = self.data_dir.join(parent_run_id).join("checkpoints");
        let latest = std::fs::read_dir(&ckpt_dir)
            .ok()
            .and_then(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
                    .max_by_key(|e| e.file_name())
            });
        let Some(latest) = latest else {
            tracing::warn!(parent = %parent_run_id, "no parent checkpoint to append sub_run_link to");
            return;
        };
        let path = latest.path();
        let s = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut ckpt: Checkpoint = match serde_json::from_str(&s) {
            Ok(c) => c,
            Err(_) => return,
        };
        ckpt.sub_run_links.push(link.clone());
        let _ = std::fs::write(&path, serde_json::to_string(&ckpt).unwrap());
    }

    /// Read the status field from `<data_dir>/<parent_run_id>/sub_runs/<sub_run_id>/run.json`.
    /// Returns an empty string if the file is missing or malformed — callers
    /// treat the empty string as "sub-run not yet started" rather than an error.
    pub fn read_sub_run_status(&self, parent_run_id: &str, sub_run_id: &str) -> String {
        let path = self.sub_run_run_json(parent_run_id, sub_run_id);
        let s = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return String::new(),
        };
        let v: serde_json::Value = serde_json::from_str(&s).unwrap_or(serde_json::Value::Null);
        v.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string()
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
            sub_run_links: vec![],
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

    // ---- Task 8: sub-run persistence helpers ----

    #[test]
    fn create_sub_run_dir_creates_nested_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p = RunPersistence::with_data_dir(tmp.path().to_path_buf());
        p.create_sub_run_dir("parent-1", "sub-2").unwrap();
        let path = tmp.path().join("parent-1").join("sub_runs").join("sub-2");
        assert!(path.exists());
    }

    #[test]
    fn append_sub_run_link_writes_to_parent_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let p = RunPersistence::with_data_dir(tmp.path().to_path_buf());
        p.create_sub_run_dir("parent-1", "sub-2").unwrap();

        // Manually create a parent checkpoint
        let ckpt_dir = tmp.path().join("parent-1").join("checkpoints");
        std::fs::create_dir_all(&ckpt_dir).unwrap();
        let ckpt = Checkpoint {
            index: 1,
            round: 1,
            phase: CheckpointPhase::Task,
            graph_snapshot: Graph::new(),
            transcript: vec![],
            sub_run_links: vec![],
        };
        let ckpt_path = ckpt_dir.join("0001.json");
        std::fs::write(&ckpt_path, serde_json::to_string(&ckpt).unwrap()).unwrap();

        let link = SubRunLink {
            node_id: crate::graph::NodeId::from("design-modules"),
            sub_run_id: "sub-2".into(),
            sub_status: "running".into(),
            created_at: 1000,
        };
        p.append_sub_run_link("parent-1", &link);

        let ckpt_back: Checkpoint = serde_json::from_str(&std::fs::read_to_string(&ckpt_path).unwrap()).unwrap();
        assert_eq!(ckpt_back.sub_run_links.len(), 1);
        assert_eq!(ckpt_back.sub_run_links[0].sub_run_id, "sub-2");
    }

    #[test]
    fn read_sub_run_status_returns_done_for_completed_run() {
        let tmp = tempfile::tempdir().unwrap();
        let p = RunPersistence::with_data_dir(tmp.path().to_path_buf());
        p.create_sub_run_dir("parent-1", "sub-2");
        let run_json_path = tmp.path().join("parent-1").join("sub_runs").join("sub-2").join("run.json");
        std::fs::write(&run_json_path, r#"{"status":"Done"}"#).unwrap();
        let s = p.read_sub_run_status("parent-1", "sub-2");
        assert_eq!(s, "Done");
    }
}
