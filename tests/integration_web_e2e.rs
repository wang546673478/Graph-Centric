//! End-to-end test: drive the full HTTP surface of the web gateway.
//! Boots axum on a real loopback port (random), POSTs a run, GETs its
//! metadata, cancels, and verifies basic shapes.

use std::sync::Arc;
use tempfile::TempDir;
use tokio::net::TcpListener;

use graph_harness::skills::storage::LocalSkillStorage;
use graph_harness::web::state::{EngineConfig, WebConfig};
use graph_harness::web::WebState;

async fn boot_server() -> (String, TempDir) {
    let dir = TempDir::new().unwrap();
    let local = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = WebConfig {
        bind_addr: addr.to_string(),
        static_dir: String::new(),
        project_root: dir.path().to_path_buf(),
        engine: EngineConfig::default(),
    };
    let state = WebState::new(local, cfg);
    let app = graph_harness::web::router(state, "");
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), dir)
}

#[tokio::test]
async fn health_endpoint_works_over_real_socket() {
    let (base, _dir) = boot_server().await;
    let resp = reqwest::get(format!("{base}/api/health")).await.unwrap();
    assert!(resp.status().is_success());
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn create_run_then_get_run_lifecycle() {
    // Set env so ModelConfig::load() succeeds without panicking inside
    // drive_run. The model itself won't be called because the proposer
    // won't be reached if config errors out — but for a smoke test
    // we only care about the HTTP shape.
    unsafe {
        std::env::set_var("MODEL_BASE_URL", "http://stub.invalid");
        std::env::set_var("MODEL_NAME_FAST", "test-fast");
        std::env::set_var("MODEL_NAME_DEEP", "test-deep");
    }

    let (base, _dir) = boot_server().await;
    let client = reqwest::Client::new();

    // Create a run.
    let body = serde_json::json!({"task": "test task"});
    let resp = client
        .post(format!("{base}/api/runs"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "create_run failed: {:?}", resp.status());
    let json: serde_json::Value = resp.json().await.unwrap();
    let id = json["id"].as_str().expect("id missing").to_string();
    assert!(!id.is_empty());

    // Get the run.
    let resp = client
        .get(format!("{base}/api/runs/{id}"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let meta: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(meta["id"], id);
    assert_eq!(meta["task"], "test task");

    // Cancel the run (idempotent — running or already-done).
    let resp = client
        .delete(format!("{base}/api/runs/{id}"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "cancel failed: {:?}", resp.status());

    // List runs and confirm it's there.
    let resp = client.get(format!("{base}/api/runs")).send().await.unwrap();
    let runs: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(
        runs.iter().any(|r| r["id"] == id),
        "newly-created run should appear in list"
    );
}

#[tokio::test]
async fn list_skills_returns_empty_list() {
    let (base, _dir) = boot_server().await;
    let resp = reqwest::get(format!("{base}/api/skills")).await.unwrap();
    assert!(resp.status().is_success());
    let skills: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(skills.is_empty());
}
