//! Repo storage: read at `<project_root>/skills/`.

use super::storage::{list_skill_refs, load_skill_at, save_skill_at, SkillStorage};
use super::types::{Result, Skill};
use std::path::PathBuf;

/// Read-only-ish storage at the repo root (`<project>/skills/`).
/// The harness treats this as "approved" skills; new saves go to local.
/// `save` is implemented for symmetry (allows direct writes for tooling
/// or tests) but is not called by `capture_skill` in v1.
pub struct RepoSkillStorage {
    root: PathBuf,
}

impl RepoSkillStorage {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl SkillStorage for RepoSkillStorage {
    fn list(&self) -> Result<Vec<super::types::SkillRef>> {
        list_skill_refs(&self.root)
    }

    fn load(&self, slug: &str) -> Result<Skill> {
        load_skill_at(&self.root.join(slug))
    }

    fn save(&self, skill: &Skill) -> Result<()> {
        // Delegate to the same helper used by LocalSkillStorage. This
        // writes 5 files under `<root>/<slug>/`. The harness never calls
        // this in v1, but tooling might.
        save_skill_at(&self.root.join(&skill.slug), skill)
    }

    fn local_root(&self) -> Option<PathBuf> {
        None
    }

    fn repo_root(&self) -> PathBuf {
        self.root.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use crate::skills::types::{Skill, SkillMeta};

    fn empty_skill(slug: &str) -> Skill {
        Skill {
            slug: slug.to_string(),
            task: "do the thing".to_string(),
            trigger: "This skill applies when needed.".to_string(),
            graph: Graph::new(),
            review: serde_json::json!({}),
            meta: SkillMeta {
                created_at: "2026-06-03T00:00:00Z".to_string(),
                task_id: None,
                model_used: "test".to_string(),
                domain_tags: vec![],
                l1_avg_confidence: 0.0,
            },
        }
    }

    #[test]
    fn repo_storage_round_trips_skill() {
        let dir = tempfile::tempdir().unwrap();
        let storage = RepoSkillStorage::new(dir.path().to_path_buf());
        storage.save(&empty_skill("r1")).unwrap();
        let loaded = storage.load("r1").unwrap();
        assert_eq!(loaded.slug, "r1");
    }

    #[test]
    fn repo_storage_list_returns_empty_for_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        let storage = RepoSkillStorage::new(dir.path().join("nope"));
        assert_eq!(storage.list().unwrap(), Vec::new());
    }

    #[test]
    fn repo_storage_local_root_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let storage = RepoSkillStorage::new(dir.path().to_path_buf());
        assert_eq!(storage.local_root(), None);
    }
}
