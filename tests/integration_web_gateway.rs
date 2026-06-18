//! End-to-end tests for the web gateway. Use `axum::Router::oneshot` to
//! dispatch requests in-process — no real network binding.

use std::sync::Arc;
use tempfile::TempDir;

use graph_harness::skills::storage::LocalSkillStorage;
use graph_harness::web::state::{EngineConfig, WebConfig};
use graph_harness::web::WebState;

fn make_state() -> (TempDir, Arc<WebState>) {
    let dir = TempDir::new().unwrap();
    let local = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
    let cfg = WebConfig {
        bind_addr: "0.0.0.0:0".to_string(),
        static_dir: String::new(),
        project_root: dir.path().to_path_buf(),
        engine: EngineConfig::default(),
    };
    (dir, Arc::new(WebState::new(local, cfg)))
}

#[tokio::test]
async fn health_endpoint_returns_ok() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let (_dir, state) = make_state();
    let app = graph_harness::web::router((*state).clone(), "");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("\"ok\""));
}

#[tokio::test]
async fn list_skills_returns_empty_initially() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let (_dir, state) = make_state();
    let app = graph_harness::web::router((*state).clone(), "");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/skills")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.is_array());
    assert!(v.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn get_skill_404_for_missing_slug() {
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let (_dir, state) = make_state();
    let app = graph_harness::web::router((*state).clone(), "");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/skills/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_run_returns_uuid_id() {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let (_dir, state) = make_state();
    let app = graph_harness::web::router((*state).clone(), "");

    let body = serde_json::to_vec(&serde_json::json!({"task": "do nothing"})).unwrap();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runs")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = to_bytes(resp.into_body(), 4096).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(v["id"].is_string());
    let id = v["id"].as_str().unwrap();
    assert!(!id.is_empty());
}

#[tokio::test]
async fn get_run_404_for_missing_id() {
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let (_dir, state) = make_state();
    let app = graph_harness::web::router((*state).clone(), "");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/runs/00000000-0000-0000-0000-000000000000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
