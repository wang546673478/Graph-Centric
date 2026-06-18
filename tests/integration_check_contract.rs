//! End-to-end integration test: a sub-task with a KnowHow contract that
//! the sub-agent fails to satisfy results in a `success: false` from the
//! dispatcher, with a `contract violated: ...` or `max_steps (...)`
//! error string.
//!
//! Exercises the full SubTask → SubAgent → Dispatcher pipeline with
//! Task 4's contract re-check enabled.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use graph_harness::agent::contract::CheckContract;
use graph_harness::agent::dispatcher::Dispatcher;
use graph_harness::agent::subagent::{SubAgent, SubTask};
use graph_harness::context::{InMemorySources, SourceLoader};
use graph_harness::domain::TaskNeeds;
use graph_harness::graph::{Graph, NodeId};
use graph_harness::model::{FinishReason, Model, ModelRequest, ModelResponse, Usage};

/// Scripted model: pops a canned response off the queue on every call,
/// defaulting to `"ok"` (plain text — graceful-degradation path in the
/// sub-agent's parse_action, which returns `success: true` with the raw
/// content). Mirrors the dispatcher's `CountingModel` shape: tests need
/// that graceful-degradation path so the dispatcher can see the
/// contract violation and re-flag the result.
struct ScriptedModel {
    queue: Mutex<VecDeque<String>>,
}

impl ScriptedModel {
    fn new(responses: Vec<&str>) -> Self {
        Self {
            queue: Mutex::new(responses.into_iter().rev().map(String::from).collect()),
        }
    }
}

#[async_trait]
impl Model for ScriptedModel {
    fn name(&self) -> &str {
        "scripted"
    }
    async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse, graph_harness::error::HarnessError> {
        let content = self
            .queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "ok".to_string());
        Ok(ModelResponse {
            content,
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            reasoning_content: None,
            usage: Usage::default(),
        })
    }
}

fn empty_loader() -> Arc<dyn SourceLoader> {
    Arc::new(InMemorySources(HashMap::new()))
}

#[tokio::test]
async fn end_to_end_knowhow_contract_failure_marks_dispatch_failed() {
    // Sub-agent ignores the contract and emits a wrong `final_answer`
    // on step 0 (contract check fails; retry fed back). On step 1 the
    // queue is empty so the model returns plain text "ok" — that
    // doesn't parse as a JSON action, so the sub-agent takes the
    // graceful-degradation path and returns `success: true` with
    // output "ok". The dispatcher's re-check then sees a successful
    // result that doesn't mention "auth.rs" and re-flags it as
    // "contract violated: ...". So the outcome carries a contract
    // error and `all_succeeded = false`.
    let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![
        r#"{"action":"final_answer","answer":"nothing useful","thinking":""}"#,
    ]));
    let agent = Arc::new(SubAgent::new(model).with_max_steps(2));
    let d = Dispatcher::new(agent).with_max_concurrent(1);

    let st = SubTask {
        id: NodeId::from("t1"),
        description: "find auth".into(),
        involved_nodes: vec![],
        needs: TaskNeeds::default(),
        contract: CheckContract::KnowHow {
            must_mention_any: vec!["auth.rs".into()],
            min_length: 5,
        },
        role_prompt: String::new(),
    };

    let mut g = Graph::new();
    g.add_node(st.to_task_node());

    let outcome = d.run(&g, &Graph::new(), empty_loader()).await.unwrap();

    assert_eq!(outcome.results.len(), 1);
    assert!(
        !outcome.all_succeeded,
        "dispatcher should mark contract violation as failure"
    );
    let err = outcome.results[0].error.as_deref().unwrap_or("");
    assert!(
        err.contains("contract") || err.contains("max_steps"),
        "expected contract or max_steps in error, got: {err}"
    );
}
