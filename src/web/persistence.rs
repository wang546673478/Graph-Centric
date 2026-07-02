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
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// v2 spec §5.6: summary file format used by `compact_checkpoints`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointsCompact {
    pub run_id: String,
    pub generated_at_ms: u64,
    pub entries: Vec<CompactEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactEntry {
    pub index: usize,
    pub round: usize,
    pub phase: String,
    pub node_count: usize,
    pub edge_count: usize,
    pub ts_unix_ms: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            std::fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

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

    pub(crate) fn checkpoints_dir(&self, run_id: &str) -> PathBuf {
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

    // -----------------------------------------------------------------------
    // v2 spec §5.6: persistence maintenance
    // -----------------------------------------------------------------------

    /// Compress old checkpoints into a single `checkpoints.compact.json`
    /// summary, deleting the per-index files. Keeps the first
    /// `keep` and last `keep_tail` checkpoints verbatim (they're
    /// the most likely to be re-loaded by the user) and replaces
    /// the rest with a single-line-per-checkpoint summary.
    ///
    /// The compact file has the same JSON shape as
    /// `CheckpointsCompact` below: just the metadata, no full
    /// `graph_snapshot` or `transcript` payloads.
    ///
    /// Returns the number of checkpoints that were compacted.
    pub fn compact_checkpoints(
        &self,
        run_id: &str,
        keep: usize,
        keep_tail: usize,
    ) -> std::io::Result<usize> {
        let dir = self.checkpoints_dir(run_id);
        if !dir.exists() {
            return Ok(0);
        }
        // List all per-checkpoint files sorted by index.
        let mut entries: Vec<(usize, std::path::PathBuf)> = Vec::new();
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            let p = e.path();
            if p.extension().map_or(false, |x| x == "json") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    if let Ok(idx) = stem.parse::<usize>() {
                        entries.push((idx, p));
                    }
                }
            }
        }
        entries.sort_by_key(|(idx, _)| *idx);
        if entries.len() <= keep + keep_tail {
            return Ok(0);
        }
        // Decide which to keep verbatim and which to compact.
        let to_compact: Vec<(usize, std::path::PathBuf)> = entries
            .iter()
            .enumerate()
            .filter(|(i, _)| *i >= keep && *i < entries.len() - keep_tail)
            .map(|(_, e)| e.clone())
            .collect();
        if to_compact.is_empty() {
            return Ok(0);
        }
        // Build a summary file.
        let mut compact = CheckpointsCompact {
            run_id: run_id.to_string(),
            generated_at_ms: now_ms(),
            entries: Vec::new(),
        };
        for (idx, _) in &to_compact {
            if let Ok(Some(cp)) = self.load_checkpoint(run_id, *idx) {
                compact.entries.push(CompactEntry {
                    index: cp.index,
                    round: cp.round,
                    phase: cp.phase.to_string(),
                    node_count: cp.graph_snapshot.node_count(),
                    edge_count: cp.graph_snapshot.edge_count(),
                    ts_unix_ms: now_ms(),
                });
            }
        }
        // Write the summary.
        let compact_path = dir.join("checkpoints.compact.json");
        let json = serde_json::to_string_pretty(&compact)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(&compact_path, json)?;
        // Delete the per-index files that were just summarised.
        for (_, path) in &to_compact {
            let _ = std::fs::remove_file(path);
        }
        Ok(to_compact.len())
    }

    /// v2 spec §5.6: cleanup policy. Runs that are older than
    /// `archive_after_days` and still in a terminal state get
    /// moved to a `data/runs-archive/` directory. Runs older
    /// than `purge_after_days` get deleted entirely. Returns
    /// (archived_count, purged_count).
    pub fn cleanup_runs(
        &self,
        archive_after_days: u32,
        purge_after_days: u32,
    ) -> std::io::Result<(usize, usize)> {
        if !self.data_dir.exists() {
            return Ok((0, 0));
        }
        let archive_ms = (archive_after_days as u64) * 86_400_000;
        let purge_ms = (purge_after_days as u64) * 86_400_000;
        let now = now_ms();
        let mut archived = 0;
        let mut purged = 0;
        for e in std::fs::read_dir(&self.data_dir)? {
            let e = e?;
            if !e.file_type()?.is_dir() {
                continue;
            }
            let run_dir = e.path();
            let run_json = run_dir.join("run.json");
            if !run_json.exists() {
                continue;
            }
            let age_ms = match run_dir
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default())
            {
                Ok(d) => now.saturating_sub(d.as_millis() as u64),
                Err(_) => continue,
            };
            if age_ms >= purge_ms {
                let _ = std::fs::remove_dir_all(&run_dir);
                purged += 1;
            } else if age_ms >= archive_ms {
                let archive_dir = self
                    .data_dir
                    .parent()
                    .unwrap_or(&self.data_dir)
                    .join("runs-archive");
                std::fs::create_dir_all(&archive_dir)?;
                let dest = archive_dir.join(e.file_name());
                let _ = std::fs::rename(&run_dir, &dest);
                archived += 1;
            }
        }
        Ok((archived, purged))
    }

    /// v2 spec §5.6: backup a critical run to a second location.
    /// Currently a simple `cp -r` analog. Returns the backup path
    /// on success. The caller decides which runs are "critical"
    /// (e.g. those marked as skills, or run by a heartbeat).
    pub fn backup_run(&self, run_id: &str, backup_root: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
        let src = self.run_dir(run_id);
        if !src.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("run {run_id} not found"),
            ));
        }
        let dest = backup_root.join(run_id);
        copy_dir_recursive(&src, &dest)?;
        Ok(dest)
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

    #[test]
    fn compact_checkpoints_replaces_middle_with_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let p = RunPersistence::new(tmp.path());
        // Save 10 checkpoints.
        for i in 0..10 {
            let cp = super::super::checkpoint::Checkpoint {
                index: i,
                round: i,
                phase: super::super::checkpoint::CheckpointPhase::Graph,
                graph_snapshot: crate::graph::Graph::new(),
                transcript: vec![],
                sub_run_links: vec![],
            };
            p.save_checkpoint("run1", &cp).unwrap();
        }
        // Compact keeping first 2 + last 2 verbatim.
        let compacted = p.compact_checkpoints("run1", 2, 2).unwrap();
        assert_eq!(compacted, 6, "expected 6 compacted (kept 2+2=4 of 10)");
        // The compact file should exist and have 6 entries.
        let compact_path = p
            .checkpoints_dir("run1")
            .join("checkpoints.compact.json");
        assert!(
            compact_path.exists(),
            "compact file missing at {}",
            compact_path.display()
        );
        let compact: CheckpointsCompact =
            serde_json::from_str(&std::fs::read_to_string(&compact_path).unwrap()).unwrap();
        assert_eq!(compact.entries.len(), 6);
    }

    #[test]
    fn backup_run_copies_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let p = RunPersistence::new(tmp.path());
        let meta = RunMetadata {
            id: "abc".into(),
            task: "t".into(),
            status: RunStatus::Running,
            duration_ms: 0,
            captured_skill: None,
            tokens_used: 0,
        };
        p.save_run_meta(&meta).unwrap();
        let backup = tmp.path().join("backups");
        std::fs::create_dir_all(&backup).unwrap();
        let dest = p.backup_run("abc", &backup).unwrap();
        assert!(dest.join("run.json").exists());
    }
}
