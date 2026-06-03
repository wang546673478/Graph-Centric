//! `/api/skills/*` HTTP handlers.

use super::errors::ApiError;
use crate::skills::{
    storage::{LocalSkillStorage, SkillStorage},
    RepoSkillStorage, Skill, SkillRef,
};
use crate::web::WebState;
use axum::{
    extract::{Path, State},
    Json,
};
use std::sync::Arc;

type AppState = State<Arc<WebState>>;

pub async fn list_skills(State(state): AppState) -> Result<Json<Vec<SkillRef>>, ApiError> {
    let list = state.skills.list()?;
    Ok(Json(list))
}

pub async fn get_skill(
    State(state): AppState,
    Path(slug): Path<String>,
) -> Result<Json<Skill>, ApiError> {
    let skill = state.skills.load(&slug)?;
    Ok(Json(skill))
}

pub async fn promote_skill(
    State(state): AppState,
    Path(slug): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Load the skill (from composite, which prefers local), then save to
    // the repo storage directly.
    let skill = state.skills.load(&slug)?;
    let repo_root = state.config.project_root.join("skills");
    let repo = RepoSkillStorage::new(repo_root);
    repo.save(&skill)?;
    Ok(Json(serde_json::json!({"promoted": true})))
}

pub async fn delete_skill(
    State(state): AppState,
    Path(slug): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // SkillStorage trait has no delete() — go via filesystem. Best-effort
    // across local + repo roots. Either missing → not found.
    delete_skill_at(&state.config, &slug)?;
    Ok(Json(serde_json::json!({"deleted": true})))
}

/// Delete a skill directory under both local and repo roots. Returns
/// `NotFound` if neither location had the skill.
fn delete_skill_at(config: &super::state::WebConfig, slug: &str) -> Result<(), ApiError> {
    let local_root = LocalSkillStorage::default_install()
        .and_then(|s| s.local_root())
        .unwrap_or_else(|| std::env::temp_dir().join("graph-centric-skills-fallback"));
    let local = local_root.join(slug);
    let mut repo = config.project_root.clone();
    repo.push("skills");
    repo.push(slug);

    let local_existed = local.exists();
    let local_removed = std::fs::remove_dir_all(&local).is_ok();

    let repo_existed = repo.exists();
    let repo_removed = std::fs::remove_dir_all(&repo).is_ok();

    if !local_existed && !repo_existed {
        return Err(ApiError::NotFound(slug.to_string()));
    }
    // If we found it somewhere but removal failed, surface as Internal.
    if (local_existed && !local_removed) || (repo_existed && !repo_removed) {
        return Err(ApiError::Internal(format!(
            "failed to delete skill '{slug}' (local_removed={local_removed}, repo_removed={repo_removed})"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::storage::LocalSkillStorage;
    use crate::skills::types::{Skill, SkillMeta};
    use crate::graph::Graph;

    fn make_state() -> Arc<WebState> {
        let dir = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let cfg = super::super::state::WebConfig {
            bind_addr: "0.0.0.0:0".to_string(),
            static_dir: String::new(),
            project_root: dir.path().to_path_buf(),
        };
        Arc::new(super::super::WebState::new(local, cfg))
    }

    fn _sample_skill_unused(slug: &str) -> Skill {
        Skill {
            slug: slug.to_string(),
            task: "t".into(),
            trigger: "trig".into(),
            graph: Graph::new(),
            review: serde_json::json!({}),
            meta: SkillMeta {
                created_at: "2026-06-03T00:00:00Z".into(),
                task_id: None,
                model_used: "test".into(),
                domain_tags: vec![],
                l1_avg_confidence: 0.0,
            },
        }
    }

    #[tokio::test]
    async fn list_skills_returns_empty_when_no_skills() {
        let state = make_state();
        let resp = list_skills(State(state)).await.unwrap();
        assert!(resp.0.is_empty());
    }

    #[tokio::test]
    async fn get_skill_404_when_missing() {
        let state = make_state();
        let err = get_skill(State(state), Path("nope".into())).await.unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::NOT_FOUND);
    }
}
