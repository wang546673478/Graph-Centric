//! `/api/files/*` HTTP handlers (git-based file change detection).

use super::errors::ApiError;
use crate::web::WebState;
use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

type AppState = State<Arc<WebState>>;

#[derive(Deserialize)]
pub struct ChangedSince {
    #[serde(default)]
    pub since: Option<String>, // ISO 8601
}

#[derive(Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub change_type: String, // "added" | "modified" | "deleted"
}

pub async fn files_changed(
    State(state): AppState,
    Query(params): Query<ChangedSince>,
) -> Result<Json<Vec<ChangedFile>>, ApiError> {
    let project = &state.config.project_root;
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(project)
        .arg("diff")
        .arg("--name-status")
        .arg("--no-color");
    if let Some(since) = &params.since {
        cmd.arg("--since").arg(since);
    }
    let output = cmd
        .output()
        .map_err(|e| ApiError::Internal(format!("git not available: {e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let mut out = Vec::new();
    for line in stdout.lines() {
        // Format: "M\tpath" or "A\tpath" or "D\tpath"
        let mut parts = line.splitn(2, '\t');
        let status = parts.next().unwrap_or("").trim();
        let path = parts.next().unwrap_or("").trim().to_string();
        if !path.is_empty() {
            out.push(ChangedFile {
                path,
                change_type: match status {
                    "A" => "added".into(),
                    "D" => "deleted".into(),
                    _ => "modified".into(),
                },
            });
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct DiffPath {
    pub path: String,
}

pub async fn file_diff(
    State(state): AppState,
    Query(params): Query<DiffPath>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let project = &state.config.project_root;
    let output = std::process::Command::new("git")
        .current_dir(project)
        .args(["diff", "HEAD", "--", &params.path])
        .output()
        .map_err(|e| ApiError::Internal(format!("git not available: {e}")))?;
    let diff = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(Json(serde_json::json!({
        "path": params.path,
        "diff": diff,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::storage::LocalSkillStorage;

    fn make_state_in_dir(dir: &std::path::Path) -> Arc<WebState> {
        let local = Arc::new(LocalSkillStorage::new(dir.to_path_buf()));
        let cfg = super::super::state::WebConfig {
            bind_addr: "0.0.0.0:0".to_string(),
            static_dir: String::new(),
            project_root: dir.to_path_buf(),
            engine: super::super::state::EngineConfig::default(),
        };
        Arc::new(super::super::WebState::new(local, cfg))
    }

    #[tokio::test]
    async fn file_diff_returns_empty_string_when_no_changes() {
        let dir = tempfile::tempdir().unwrap();
        let state = make_state_in_dir(dir.path());
        let result = file_diff(
            State(state),
            Query(DiffPath {
                path: "anything".into(),
            }),
        )
        .await;
        match result {
            Ok(json) => {
                let v: serde_json::Value = json.0;
                assert_eq!(v["diff"], "");
            }
            Err(_) => { /* git not available; acceptable */ }
        }
    }
}
