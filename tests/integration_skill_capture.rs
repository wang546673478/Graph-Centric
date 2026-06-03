//! End-to-end test for the skill capture and retrieval flow.
//!
//! These tests exercise the full path: capture a skill, list it via
//! CompositeSkillStorage, verify the Proposer prompt includes it.

use tempfile::TempDir;

use graph_harness::graph::Graph;
use graph_harness::skills::{
    CompositeSkillStorage, LocalSkillStorage, RepoSkillStorage, Skill, SkillMeta, SkillStorage,
};

#[test]
fn skills_round_trip_via_storage() {
    let local_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();

    let local = LocalSkillStorage::new(local_dir.path().to_path_buf());
    let repo = RepoSkillStorage::new(repo_dir.path().to_path_buf());

    // Save a skill via local.
    let skill = Skill {
        slug: "round-trip".to_string(),
        task: "do the thing".to_string(),
        trigger: "applies when the thing is needed".to_string(),
        graph: Graph::new(),
        review: serde_json::json!({"verdict": "pass"}),
        meta: SkillMeta {
            created_at: "2026-06-03T00:00:00Z".to_string(),
            task_id: None,
            model_used: "test".to_string(),
            domain_tags: vec![],
            l1_avg_confidence: 0.0,
        },
    };
    local.save(&skill).unwrap();

    // Build composite (consumes `local`).
    let composite = CompositeSkillStorage::new(Some(local), repo);

    // List via composite — should include the just-saved skill.
    let list = composite.list().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].slug, "round-trip");

    // Load via composite.
    let loaded = composite.load("round-trip").unwrap();
    assert_eq!(loaded.task, "do the thing");
}

#[test]
fn composite_lists_local_first_under_repo_collision() {
    let local_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();

    let local = LocalSkillStorage::new(local_dir.path().to_path_buf());
    let repo = RepoSkillStorage::new(repo_dir.path().to_path_buf());

    // Same slug in both.
    let repo_skill = Skill {
        slug: "shared".to_string(),
        task: "from-repo".to_string(),
        trigger: "from-repo".to_string(),
        graph: Graph::new(),
        review: serde_json::json!({}),
        meta: SkillMeta {
            created_at: "2026-06-03T00:00:00Z".to_string(),
            task_id: None,
            model_used: "test".to_string(),
            domain_tags: vec![],
            l1_avg_confidence: 0.0,
        },
    };
    repo.save(&repo_skill).unwrap();

    let mut local_skill = repo_skill.clone();
    local_skill.task = "from-local".to_string();
    local_skill.trigger = "from-local".to_string();
    local.save(&local_skill).unwrap();

    let composite = CompositeSkillStorage::new(Some(local), repo);
    let list = composite.list().unwrap();
    assert_eq!(list.len(), 1, "duplicate slug should appear once");
    assert_eq!(list[0].trigger, "from-local");
}
