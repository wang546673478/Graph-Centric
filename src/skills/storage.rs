//! `SkillStorage` trait and the local-root implementation.

use super::types::{Result, Skill, SkillError, SkillRef};
use std::path::PathBuf;
use std::sync::Mutex;

/// Abstraction over a skill storage root. Implementations are `Send + Sync`
/// so they can be shared across the async runtime.
pub trait SkillStorage: Send + Sync {
    /// List all skills (slug + trigger one-liner only).
    fn list(&self) -> Result<Vec<SkillRef>>;

    /// Load a single skill by slug.
    fn load(&self, slug: &str) -> Result<Skill>;

    /// Save a skill. Implementations decide where it lands.
    fn save(&self, skill: &Skill) -> Result<()>;

    /// Path to the local root, if this storage has one. Used by tooling
    /// that wants to "promote" a local skill to repo.
    fn local_root(&self) -> Option<PathBuf>;

    /// Path to the repo root.
    fn repo_root(&self) -> PathBuf;
}

/// Local storage at `~/.local/share/graph-centric/skills/`. New skills land
/// here by default. Created lazily on first save.
pub struct LocalSkillStorage {
    root: PathBuf,
    // Serializes concurrent writes to the same slug.
    write_lock: Mutex<()>,
}

impl LocalSkillStorage {
    /// Construct with a custom root (for tests). Production callers use
    /// `LocalSkillStorage::default_install()`.
    pub fn new(root: PathBuf) -> Self {
        Self { root, write_lock: Mutex::new(()) }
    }

    /// Construct at the XDG default: `~/.local/share/graph-centric/skills/`.
    /// Returns `None` if `$HOME` is unset.
    pub fn default_install() -> Option<Self> {
        let home = std::env::var_os("HOME")?;
        let mut root = PathBuf::from(home);
        root.push(".local");
        root.push("share");
        root.push("graph-centric");
        root.push("skills");
        Some(Self::new(root))
    }

    fn skill_dir(&self, slug: &str) -> PathBuf {
        self.root.join(slug)
    }
}

impl SkillStorage for LocalSkillStorage {
    fn list(&self) -> Result<Vec<SkillRef>> {
        list_skill_refs(&self.root)
    }

    fn load(&self, slug: &str) -> Result<Skill> {
        load_skill_at(&self.skill_dir(slug))
    }

    fn save(&self, skill: &Skill) -> Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        save_skill_at(&self.skill_dir(&skill.slug), skill)
    }

    fn local_root(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }

    fn repo_root(&self) -> PathBuf {
        // Local has no repo root; return the local root for symmetry.
        self.root.clone()
    }
}

// ---- Helpers shared with RepoSkillStorage ----

pub(crate) fn list_skill_refs(root: &std::path::Path) -> Result<Vec<SkillRef>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let trigger_path = entry.path().join("trigger.md");
        if !trigger_path.exists() {
            continue;
        }
        let trigger = std::fs::read_to_string(&trigger_path)
            .map_err(SkillError::Io)?;
        let slug = entry.file_name().to_string_lossy().to_string();
        out.push(SkillRef { slug, trigger: trigger.trim().to_string() });
    }
    Ok(out)
}

pub(crate) fn load_skill_at(skill_dir: &std::path::Path) -> Result<Skill> {
    if !skill_dir.exists() {
        let slug = skill_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        return Err(SkillError::NotFound(slug));
    }
    let graph_path = skill_dir.join("graph.json");
    let task_path = skill_dir.join("task.md");
    let trigger_path = skill_dir.join("trigger.md");
    let review_path = skill_dir.join("review.json");
    let meta_path = skill_dir.join("meta.json");

    let graph_json = std::fs::read_to_string(&graph_path)?;
    let graph: crate::graph::Graph = serde_json::from_str(&graph_json)?;
    let task = std::fs::read_to_string(&task_path)?;
    let trigger = std::fs::read_to_string(&trigger_path)?;
    let review: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&review_path)?,
    )?;
    let meta: super::types::SkillMeta = serde_json::from_str(
        &std::fs::read_to_string(&meta_path)?,
    )?;

    let slug = skill_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(Skill {
        slug,
        task: task.trim().to_string(),
        trigger: trigger.trim().to_string(),
        graph,
        review,
        meta,
    })
}

pub(crate) fn save_skill_at(skill_dir: &std::path::Path, skill: &Skill) -> Result<()> {
    std::fs::create_dir_all(skill_dir)?;
    std::fs::write(skill_dir.join("task.md"), &skill.task)?;
    std::fs::write(skill_dir.join("trigger.md"), &skill.trigger)?;
    std::fs::write(
        skill_dir.join("graph.json"),
        serde_json::to_string_pretty(&skill.graph)?,
    )?;
    std::fs::write(
        skill_dir.join("review.json"),
        serde_json::to_string_pretty(&skill.review)?,
    )?;
    std::fs::write(
        skill_dir.join("meta.json"),
        serde_json::to_string_pretty(&skill.meta)?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;

    fn empty_skill(slug: &str) -> Skill {
        Skill {
            slug: slug.to_string(),
            task: "do the thing".to_string(),
            trigger: "This skill applies when the thing is needed.".to_string(),
            graph: Graph::new(),
            review: serde_json::json!({"verdict": "pass"}),
            meta: super::super::types::SkillMeta {
                created_at: "2026-06-03T00:00:00Z".to_string(),
                task_id: None,
                model_used: "test".to_string(),
                domain_tags: vec![],
                l1_avg_confidence: 0.0,
            },
        }
    }

    #[test]
    fn local_storage_creates_root_on_first_save() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("nested/skills");
        let storage = LocalSkillStorage::new(root.clone());
        storage.save(&empty_skill("foo")).unwrap();
        assert!(root.exists(), "local root should be created on first save");
    }

    #[test]
    fn local_storage_round_trips_skill() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalSkillStorage::new(dir.path().to_path_buf());
        let original = empty_skill("round-trip");
        storage.save(&original).unwrap();
        let loaded = storage.load("round-trip").unwrap();
        assert_eq!(loaded.slug, original.slug);
        assert_eq!(loaded.task, original.task);
        assert_eq!(loaded.trigger, original.trigger);
        assert_eq!(loaded.review, original.review);
        assert_eq!(loaded.meta.model_used, original.meta.model_used);
    }

    #[test]
    fn local_storage_returns_empty_when_root_missing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("does-not-exist");
        let storage = LocalSkillStorage::new(root);
        assert_eq!(storage.list().unwrap(), Vec::new());
    }

    #[test]
    fn local_storage_load_missing_skill_errors() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalSkillStorage::new(dir.path().to_path_buf());
        let err = storage.load("nope").unwrap_err();
        assert!(matches!(err, SkillError::NotFound(_)));
    }
}
