//! End-to-end tests for the drill-down sub-graph mechanism.
//!
//! These tests live in `tests/integration_*.rs` and exercise the
//! public API surface of the GraphLoop drill-down machinery:
//!   - `apply_graph_patch_with_drill_down` (queues a fork)
//!   - `fork_sub_graph_for` (creates the sub-run, spawns the sub-loop)
//!   - `poll_sub_run_status` (transitions handle from Running → Done/Error)
//!   - `mark_complex_node_done` / `mark_complex_node_error` (stamps node metadata)
//!   - `RunPersistence` sub-run directory + `run.json` round-trip
//!
//! Strategy: rather than mocking a model and driving a full `step()`
//! loop (which would require constructing a `GraphProposer` whose
//! internal model emits a `drill_down` patch and then
//! `ready_for_verify` in the right sequence), we exercise the
//! integration through the public helpers directly. The behavior
//! under test — "patch with `drill_down` → fork → poll → mark done
//! or error" — is the exact surface that `step_graph` calls in
//! Task 10, so these tests give the same coverage with a fraction
//! of the moving parts. The `step_graph` drain loop is mirrored
//! inline in the tests via `fork_sub_graph_for` + `poll_sub_run_status`,
//! which is exactly what `step_graph` does internally.

use std::sync::Arc;
use tempfile::TempDir;

use graph_harness::agent::graph_loop::{
    DrillDownError, GraphLoop, GraphLoopConfig, SubRunStatus,
};
use graph_harness::agent::proposer::GraphProposer;
use graph_harness::agent::verifier::Verifier;
use graph_harness::graph::{DrillDownMark, Edge, GraphPatch, Node, NodeId, RelationType};
use graph_harness::model::{Model, ModelRequest, ModelResponse};
use graph_harness::tools::ToolRegistry;
use graph_harness::web::persistence::RunPersistence;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A minimal stub model that always returns
/// `{"step":"ready_for_verify"}`. It is used to construct a valid
/// `GraphLoop` (which needs a `GraphProposer` with a model) without
/// ever being called — every test below drives the loop via the
/// `apply_graph_patch_with_drill_down` + manual `poll_sub_run_status`
/// path, so the model never fires.
struct StubModel;

#[async_trait::async_trait]
impl Model for StubModel {
    fn name(&self) -> &str {
        "stub-drill-down-e2e"
    }

    async fn complete(
        &self,
        _request: ModelRequest,
    ) -> graph_harness::error::Result<ModelResponse> {
        Ok(ModelResponse {
            content: r#"{"step":"ready_for_verify","rationale":"stub"}"#.to_string(),
            tool_calls: vec![],
            finish_reason: graph_harness::model::FinishReason::Stop,
            reasoning_content: None,
            usage: graph_harness::model::Usage::default(),
        })
    }
}

/// Build a minimal `GraphLoop` rooted at a tempdir. The loop is set
/// up with:
///   - a seed graph (immutable `start` + `deliverable` joined by LeadsTo)
///   - a fixed `run_id` so tests can predict the on-disk path
///   - `max_drilldown_depth = 2` so forks from depth 0 are always allowed
fn build_loop_with_seed(path: &std::path::Path) -> GraphLoop {
    let model: Arc<dyn Model> = Arc::new(StubModel);
    let tools = Arc::new(ToolRegistry::new());
    let proposer = GraphProposer::new(model, tools.clone(), None);
    let verifier = Verifier::structural_only();
    let cfg = GraphLoopConfig::defaults_at(path.to_path_buf());
    let mut gl = GraphLoop::new(
        "design a property management system",
        proposer,
        verifier,
        None,
        tools,
        cfg,
    );
    gl.run_id = "e2e-run-001".to_string();
    gl.config.max_drilldown_depth = 2;

    // Seed graph: start (immutable) -> deliverable.
    let mut start = Node::new(
        "start",
        graph_harness::graph::NodeKind::Task,
        "start",
        "Start: current state",
    );
    start.immutable = true;
    gl.graph.add_node(start);
    gl.graph.add_node(Node::new(
        "deliverable",
        graph_harness::graph::NodeKind::Task,
        "deliverable",
        "Deliverable: the desired outcome",
    ));
    gl.graph
        .add_edge(Edge::new(
            "start",
            "deliverable",
            RelationType::LeadsTo,
            0.9,
            "seed",
        ))
        .unwrap();

    let persistence = RunPersistence::with_data_dir(path.to_path_buf());
    gl.with_persistence(persistence)
}

/// Mirror `step_graph`'s drill-down drain block inline. The actual
/// `step_graph` calls `fork_sub_graph_for` on each queued target,
/// then inserts the returned `SubRunHandle` into
/// `pending_sub_runs`. We do the same here so the test exercises
/// the exact integration path without going through the private
/// `step_graph` method.
async fn drain_fork_queue(gl: &mut GraphLoop) {
    let queued = std::mem::take(&mut gl.pending_fork_targets);
    for (complex_node, reason) in queued {
        gl.last_patch_drill_down_reasons
            .insert(complex_node.clone(), reason);
        match gl.fork_sub_graph_for(complex_node.clone()).await {
            Ok(handle) => {
                gl.pending_sub_runs.insert(complex_node.clone(), handle);
            }
            Err(DrillDownError::DepthLimit) => {
                // Already warned inside fork_sub_graph_for; drill_down dropped.
            }
        }
        gl.last_patch_drill_down_reasons.remove(&complex_node);
    }
}

/// Wait until `run.json` exists at `path`, polling every 50ms. Panics
/// after 2 seconds — the no-op sub-loop terminates in milliseconds,
/// so this is a generous safety net.
async fn wait_for_run_json(path: &std::path::Path) {
    let mut waited_ms = 0u64;
    while !path.exists() {
        if waited_ms > 2000 {
            panic!("run.json was not written within 2s at {path:?}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        waited_ms += 50;
    }
}

/// Build a patch that adds a single node WITHOUT a `drill_down` field,
/// so applying it must not create any sub-run directories.
fn patch_simple_step(node_id: &str, summary: &str) -> GraphPatch {
    GraphPatch {
        add_nodes: vec![Node::task(node_id, summary)],
        add_edges: vec![Edge::new(
            "start",
            node_id,
            RelationType::LeadsTo,
            0.9,
            "first concrete step",
        )],
        remove_node_ids: vec![],
        remove_edge_indices: vec![],
        set_l1: vec![],
        reason: "no drill-down needed".into(),
        drill_down: None,
    }
}

/// Build a patch that adds `design-modules` as a complex node AND
/// marks it for drill-down. This is the patch the model would emit
/// when it decides the "design the modules" step warrants its own
/// sub-graph.
fn patch_design_modules_with_drill_down() -> GraphPatch {
    GraphPatch {
        add_nodes: vec![Node::task(
            "design-modules",
            "Design the property management system's modules (auth, billing, maintenance, ...)",
        )],
        add_edges: vec![Edge::new(
            "start",
            "design-modules",
            RelationType::LeadsTo,
            0.9,
            "first concrete step",
        )],
        remove_node_ids: vec![],
        remove_edge_indices: vec![],
        set_l1: vec![],
        reason: "10+ sub-modules warrant drill-down".into(),
        drill_down: Some(DrillDownMark {
            target: NodeId::from("design-modules"),
            reason: "expanding the modules into concrete sub-modules".into(),
            sub_task_override: None,
        }),
    }
}

// ---------------------------------------------------------------------------
// e2e tests
// ---------------------------------------------------------------------------

/// **Simple task, no drill-down.** Applying a patch without a
/// `drill_down` field must not create any sub-run directory and
/// must not mark any node as `expanded`.
#[tokio::test]
async fn e2e_simple_task_no_drill_down() {
    let tmp = TempDir::new().unwrap();
    let mut gl = build_loop_with_seed(tmp.path());

    // Apply a simple patch (no drill_down). The helper only mutates
    // the graph; with no drill_down, nothing is queued.
    let patch = patch_simple_step("collect-requirements", "Gather requirements from stakeholders");
    gl.apply_graph_patch_with_drill_down(&patch)
        .await
        .expect("patch apply should succeed");

    // Drain the (empty) queue — this should be a no-op.
    drain_fork_queue(&mut gl).await;

    // No pending sub-runs and no expanded nodes.
    assert!(
        gl.pending_sub_runs.is_empty(),
        "no drill-down requested, so no pending sub-runs should exist; got: {:?}",
        gl.pending_sub_runs
    );
    assert!(
        gl.pending_fork_targets.is_empty(),
        "no drill-down requested, so the fork queue should be empty"
    );
    for (id, node) in gl.graph.nodes.iter() {
        assert!(
            !node.expanded,
            "node {id} should NOT be marked expanded when no drill-down was requested"
        );
    }

    // Crucially: no `sub_runs` directory should exist under the run dir.
    let sub_runs_dir = tmp.path().join("e2e-run-001").join("sub_runs");
    assert!(
        !sub_runs_dir.exists(),
        "sub_runs directory must not be created when no drill-down was requested; found {:?}",
        sub_runs_dir
    );
}

/// **Drill-down spawns a sub-graph.** Applying a patch with
/// `drill_down: Some(...)` followed by draining the fork queue must:
///   - add the target node to the graph
///   - create a `pending_sub_runs` entry for the target
///   - create the on-disk `sub_runs/<sub_run_id>` directory
///   - mark the target node as `expanded`
///   - write a `run.json` with `status: "Done"` after the (no-op)
///     sub-loop terminates, then poll updates the node's status to
///     `"done"` via `mark_complex_node_done`.
#[tokio::test]
async fn e2e_design_modules_drills_down_to_sub_graph() {
    let tmp = TempDir::new().unwrap();
    let mut gl = build_loop_with_seed(tmp.path());

    // Apply a patch with drill_down on `design-modules`.
    let patch = patch_design_modules_with_drill_down();
    gl.apply_graph_patch_with_drill_down(&patch)
        .await
        .expect("patch apply should succeed");

    // Drain the fork queue — this is what `step_graph` does on the
    // next tick. The `tokio::spawn` inside `fork_sub_graph_for` runs
    // the sub-loop in the background.
    drain_fork_queue(&mut gl).await;

    // 1. The complex node was added to the parent graph.
    let complex = NodeId::from("design-modules");
    assert!(
        gl.graph.nodes.contains_key(&complex),
        "design-modules should be added to the parent graph"
    );

    // 2. A pending sub-run was registered.
    assert_eq!(
        gl.pending_sub_runs.len(),
        1,
        "exactly one pending sub-run should be registered after drill-down"
    );
    let handle = gl
        .pending_sub_runs
        .get(&complex)
        .expect("pending_sub_runs entry for design-modules")
        .clone();
    assert!(
        matches!(handle.status, SubRunStatus::Running),
        "handle should start in Running; got {:?}",
        handle.status
    );

    // 3. The complex node is marked expanded.
    let node = gl.graph.nodes.get(&complex).unwrap();
    assert!(node.expanded, "complex node should be marked expanded after fork");
    assert_eq!(
        node.metadata.get("sub_run_status").and_then(|v| v.as_str()),
        Some("running")
    );
    assert_eq!(
        node.metadata.get("drill_down_depth").and_then(|v| v.as_u64()),
        Some(1),
        "drill_down_depth should be 1 (parent is depth 0)"
    );

    // 4. The on-disk sub-run directory was created under the parent.
    let sub_dir = tmp
        .path()
        .join("e2e-run-001")
        .join("sub_runs")
        .join(&handle.sub_run_id);
    assert!(
        sub_dir.exists(),
        "sub-run directory should exist at {sub_dir:?}"
    );

    // 5. Wait for the spawned sub-loop to write its run.json. The
    // NoopModel returns ready_for_verify on an empty graph which
    // passes structural verify, so the sub-loop finishes immediately.
    let run_json_path = sub_dir.join("run.json");
    wait_for_run_json(&run_json_path).await;

    // 6. Poll it — this should transition the handle to Done and
    // stamp `status="done"` on the complex node.
    let mut h = handle.clone();
    gl.poll_sub_run_status(&mut h).await;
    assert!(
        matches!(h.status, SubRunStatus::Done),
        "poll_sub_run_status should transition to Done after sub-run finishes; got {:?}",
        h.status
    );

    let node = gl.graph.nodes.get(&complex).unwrap();
    assert_eq!(
        node.metadata.get("status").and_then(|v| v.as_str()),
        Some("done"),
        "complex node should be marked status=done after successful sub-run"
    );
    assert_eq!(
        node.metadata.get("sub_run_status").and_then(|v| v.as_str()),
        Some("done"),
        "complex node sub_run_status should be updated to done"
    );

    // 7. The parent graph continues to make progress: applying another
    // patch on top still works (the parent isn't stuck after the
    // sub-run completes).
    let follow_up = patch_simple_step("implement-modules", "Implement each module");
    gl.apply_graph_patch_with_drill_down(&follow_up)
        .await
        .expect("parent should accept a follow-up patch after drill-down completes");
    assert!(
        gl.graph.nodes.contains_key(&NodeId::from("implement-modules")),
        "parent graph should still accept new nodes after drill-down"
    );
}

/// **Sub-run failure propagates to the parent.** When a forked
/// sub-run's `run.json` reports `status: "Error"`, polling it must:
///   - transition the handle to `SubRunStatus::Error`
///   - stamp `status="error"` + `error=<msg>` on the complex node
///   - record the error in the conversation so the model can react
///     on the next round.
#[tokio::test]
async fn e2e_drill_down_sub_failure_propagates() {
    let tmp = TempDir::new().unwrap();
    let mut gl = build_loop_with_seed(tmp.path());

    // Apply a patch with drill_down on `design-modules`.
    let patch = patch_design_modules_with_drill_down();
    gl.apply_graph_patch_with_drill_down(&patch)
        .await
        .expect("patch apply should succeed");

    // Drain the fork queue to actually fork the sub-loop.
    drain_fork_queue(&mut gl).await;

    let complex = NodeId::from("design-modules");
    assert_eq!(gl.pending_sub_runs.len(), 1);
    let handle = gl
        .pending_sub_runs
        .get(&complex)
        .expect("pending_sub_runs entry for design-modules")
        .clone();

    // The sub-run's run.json will be written by the spawned no-op
    // sub-loop. Wait for it, then overwrite with an Error payload
    // to simulate reviewer failure.
    let sub_dir = tmp
        .path()
        .join("e2e-run-001")
        .join("sub_runs")
        .join(&handle.sub_run_id);
    let run_json_path = sub_dir.join("run.json");
    wait_for_run_json(&run_json_path).await;
    std::fs::write(
        &run_json_path,
        r#"{"status":"Error","error":"reviewer failed verification"}"#,
    )
    .expect("overwrite sub-run run.json with Error");

    // Poll — should transition the handle to Error and stamp the node.
    let mut h = handle.clone();
    gl.poll_sub_run_status(&mut h).await;

    assert!(
        matches!(h.status, SubRunStatus::Error(_)),
        "poll_sub_run_status should transition to Error after sub-run reports Error; got {:?}",
        h.status
    );

    let node = gl.graph.nodes.get(&complex).unwrap();
    assert_eq!(
        node.metadata.get("status").and_then(|v| v.as_str()),
        Some("error"),
        "complex node should be marked status=error after sub-run failure"
    );
    assert_eq!(
        node.metadata.get("error").and_then(|v| v.as_str()),
        Some("reviewer failed verification"),
        "complex node should record the sub-run's error message"
    );

    // The conversation transcript should mention the failure so the
    // model can react on the next round.
    let transcript = gl.conversation.transcript();
    assert!(
        transcript.contains("drill_down failed")
            && transcript.contains("reviewer failed verification"),
        "transcript should record a 'drill_down failed' line for the model to react to; got:\n{transcript}"
    );

    // Sanity check: depth limit still works after this failure (the
    // error path doesn't leave state in a broken form). The parent
    // should still be able to fork again at depth 0.
    let result = gl.fork_sub_graph_for(complex.clone()).await;
    assert!(
        result.is_ok(),
        "parent should still be able to fork again after a child failure (got {:?})",
        result.err()
    );
}