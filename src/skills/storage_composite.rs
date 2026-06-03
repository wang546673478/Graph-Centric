//! Composite storage: combines local + repo, with local-first dedup by slug.

use super::storage::{LocalSkillStorage, SkillStorage};
use super::types::{Result, Skill, SkillError, SkillRef};
use std::collections::HashSet;

/// Combines an optional local storage and a repo storage. `list()` returns
/// local entries first; on slug collision, the local version wins.
///
/// `save()` is intentionally NOT exposed: new saves always go to local
/// (call `LocalSkillStorage::save` directly), and the user promotes
/// via filesystem.
pub struct CompositeSkillStorage {
    local: Option<LocalSkillStorage>,
    repo: super::storage_repo::RepoSkillStorage,
}

impl CompositeSkillStorage {
    pub fn new(local: Option<LocalSkillStorage>, repo: super::storage_repo::RepoSkillStorage) -> Self {
        Self { local, repo }
    }
}

impl SkillStorage for CompositeSkillStorage {
    fn list(&self) -> Result<Vec<SkillRef>> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<SkillRef> = Vec::new();

        // Local first.
        if let Some(local) = &self.local {
            for r in local.list()? {
                if seen.insert(r.slug.clone()) {
                    out.push(r);
                }
            }
        }
        // Then repo, skipping slugs already seen.
        for r in self.repo.list()? {
            if seen.insert(r.slug.clone()) {
                out.push(r);
            }
        }
        Ok(out)
    }

    fn load(&self, slug: &str) -> Result<Skill> {
        // Local first, fall back to repo.
        if let Some(local) = &self.local {
            if let Ok(skill) = local.load(slug) {
                return Ok(skill);
            }
        }
        self.repo.load(slug)
    }

    fn save(&self, _skill: &Skill) -> Result<()> {
        Err(SkillError::Model(
            "CompositeSkillStorage::save is not supported; use LocalSkillStorage::save".into()
        ))
    }

    fn local_root(&self) -> Option<std::path::PathBuf> {
        self.local.as_ref().and_then(|l| l.local_root())
    }

    fn repo_root(&self) -> std::path::PathBuf {
        self.repo.repo_root()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use crate::skills::storage_repo::RepoSkillStorage;
    use crate::skills::types::{Skill, SkillMeta};
    use std::path::PathBuf;

    fn empty_skill(slug: &str) -> Skill {
        Skill {
            slug: slug.to_string(),
            task: "task".to_string(),
            trigger: "trigger".to_string(),
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

    fn composite_with_both(local_root: PathBuf, repo_root: PathBuf) -> CompositeSkillStorage {
        CompositeSkillStorage::new(
            Some(LocalSkillStorage::new(local_root)),
            RepoSkillStorage::new(repo_root),
        )
    }

    #[test]
    fn composite_storage_lists_local_first() {
        let local = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let c = composite_with_both(local.path().to_path_buf(), repo.path().to_path_buf());
        c.repo.save(&empty_skill("a")).unwrap();
        c.local.as_ref().unwrap().save(&empty_skill("b")).unwrap();

        let list = c.list().unwrap();
        // Local-first ordering: b (local) before a (repo).
        assert_eq!(list[0].slug, "b");
        assert_eq!(list[1].slug, "a");
    }

    #[test]
    fn composite_storage_dedupes_by_slug() {
        let local = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let c = composite_with_both(local.path().to_path_buf(), repo.path().to_path_buf());
        c.repo.save(&empty_skill("dup")).unwrap();
        c.local.as_ref().unwrap().save(&empty_skill("dup")).unwrap();

        let list = c.list().unwrap();
        assert_eq!(list.len(), 1, "duplicate slug should appear once");
        assert_eq!(list[0].slug, "dup");
    }

    #[test]
    fn composite_storage_load_prefers_local() {
        let local = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let c = composite_with_both(local.path().to_path_buf(), repo.path().to_path_buf());

        let mut repo_skill = empty_skill("x");
        repo_skill.trigger = "from-repo".to_string();
        c.repo.save(&repo_skill).unwrap();

        let mut local_skill = empty_skill("x");
        local_skill.trigger = "from-local".to_string();
        c.local.as_ref().unwrap().save(&local_skill).unwrap();

        let loaded = c.load("x").unwrap();
        assert_eq!(loaded.trigger, "from-local");
    }

    #[test]
    fn composite_storage_load_falls_back_to_repo() {
        let local = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let c = composite_with_both(local.path().to_path_buf(), repo.path().to_path_buf());
        c.repo.save(&empty_skill("only-in-repo")).unwrap();

        let loaded = c.load("only-in-repo").unwrap();
        assert_eq!(loaded.slug, "only-in-repo");
    }

    #[test]
    fn composite_storage_save_errors() {
        let local = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let c = composite_with_both(local.path().to_path_buf(), repo.path().to_path_buf());
        let err = c.save(&empty_skill("x")).unwrap_err();
        assert!(matches!(err, SkillError::Model(_)));
    }
}
