//! End-to-end tests for the new tool guards.
//!
//! These tests use real `SubAgent` + `ToolRegistry` + `BashTool` +
//! `DangerousCommandDeny` (the new default) + an optional `ScopeGuard`,
//! driven by a `MockModel` (defined inline to avoid leaking test
//! fixtures across the codebase).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use graph_harness::agent::contract::CheckContract;
use graph_harness::agent::subagent::{SubAgent, SubTask};
use graph_harness::context::{InMemorySources, SourceLoader};
use graph_harness::error::HarnessError;
use graph_harness::graph::{Graph, Node, NodeId};
use graph_harness::model::{FinishReason, Model, ModelRequest, ModelResponse, Usage};
use graph_harness::tools::{BashTool, ScopeGuard, ToolRegistry};

struct MockModel {
    responses: Mutex<Vec<String>>,
}

impl MockModel {
    /// Build a mock model that emits `responses` in order: the first
    /// element of `responses` is returned on the first call, the second
    /// on the second call, etc. Internally we store them reversed so
    /// that `Vec::pop()` returns them in the right order.
    fn new(responses: Vec<&str>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().rev().map(String::from).collect()),
        }
    }
}

#[async_trait]
impl Model for MockModel {
    fn name(&self) -> &str {
        "mock"
    }
    async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse, HarnessError> {
        let content = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| {
                r#"{"action":"final_answer","answer":"default","thinking":""}"#.to_string()
            });
        Ok(ModelResponse {
            content,
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            reasoning_content: None,
            usage: Usage::default(),
        })
    }
}

fn empty_loader() -> Arc<dyn SourceLoader> {
    Arc::new(InMemorySources(HashMap::new()))
}

fn world() -> Graph {
    let mut g = Graph::new();
    g.add_node(Node::file("/proj/src/a.rs", "A"));
    g
}

fn task(involved: Vec<&str>) -> SubTask {
    SubTask {
        id: NodeId::from("t1"),
        description: "Test task".into(),
        involved_nodes: involved.into_iter().map(NodeId::from).collect(),
        needs: Default::default(),
        contract: CheckContract::default(),
        role_prompt: String::new(),
    }
}

#[tokio::test]
async fn dangerous_command_is_denied_by_default_policy() {
    // Model tries to rm -rf / — the default policy must block it.
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(BashTool::new()));
    let tools = Arc::new(reg);

    let try_rm = r#"{"action":"use_tool","tool":"bash","args":{"command":"sudo rm -rf /"},"thinking":"bad"}"#;
    let recover = r#"{"action":"final_answer","answer":"blocked","thinking":"saw denial"}"#;
    // First emission is the bash call, then the final_answer.
    let model: Arc<dyn Model> = Arc::new(MockModel::new(vec![try_rm, recover]));

    let agent = SubAgent::new(model).with_tools(tools);
    let result = agent
        .execute(
            &task(vec!["/proj/src/a.rs"]),
            &world(),
            empty_loader().as_ref(),
        )
        .await
        .unwrap();
    assert!(result.success);
    assert!(result.output.contains("blocked"));
}

#[tokio::test]
async fn scope_guard_blocks_out_of_scope_write() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(BashTool::new()));
    let tools = Arc::new(reg);
    let guard = Arc::new(ScopeGuard::new(vec![PathBuf::from("/proj/src")]));

    let try_outside = r#"{"action":"use_tool","tool":"bash","args":{"command":"rm /etc/passwd"},"thinking":"x"}"#;
    let recover = r#"{"action":"final_answer","answer":"scope said no","thinking":"got it"}"#;
    // First emission is the bash call, then the final_answer.
    let model: Arc<dyn Model> = Arc::new(MockModel::new(vec![try_outside, recover]));

    let agent = SubAgent::new(model)
        .with_tools(tools)
        .with_task_scope(guard);
    let result = agent
        .execute(
            &task(vec!["/proj/src/a.rs"]),
            &world(),
            empty_loader().as_ref(),
        )
        .await
        .unwrap();
    assert!(result.success);
    assert!(result.output.contains("scope said no"));
}

#[tokio::test]
async fn both_guards_let_through_in_scope_safe_command() {
    // Reading an in-scope file with a safe command: nothing in the
    // chain should object.
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(BashTool::new()));
    let tools = Arc::new(reg);
    let guard = Arc::new(ScopeGuard::new(vec![PathBuf::from("/proj/src")]));

    let read_ok = r#"{"action":"use_tool","tool":"bash","args":{"command":"cat /proj/src/a.rs"},"thinking":"see file"}"#;
    let finalize = r#"{"action":"final_answer","answer":"got content","thinking":"done"}"#;
    // First emission is the bash call, then the final_answer.
    let model: Arc<dyn Model> = Arc::new(MockModel::new(vec![read_ok, finalize]));

    let agent = SubAgent::new(model)
        .with_tools(tools)
        .with_task_scope(guard);
    let result = agent
        .execute(
            &task(vec!["/proj/src/a.rs"]),
            &world(),
            empty_loader().as_ref(),
        )
        .await
        .unwrap();
    assert!(result.success);
    assert!(result.output.contains("got content"));
    assert_eq!(result.tool_calls_made, 1);
}
