//! `/api/runs/*` HTTP handlers and the run driver.
//!
//! The driver spawns a tokio task that constructs a `GraphLoop` (same as
//! `bin/agent_a` does), runs `step()` in a loop, and maps each
//! `LoopState` to one or more `RunEvent`s broadcast on the session's
//! channel. Cancellation, answers, and repairs are all coordinated
//! through `tokio::sync::Notify` + the session's storage.

use super::checkpoint::{Checkpoint, CheckpointMeta, SubRunLink};
use super::errors::ApiError;
use super::events::{InitialGraphDto, RunEvent};
use super::run_session::{RunMetadata, RunSession, RunStatus};
use super::{RunId, WebState};
use super::state::EngineConfig;
use crate::agent::decomposer::Decomposer;
use crate::agent::dispatcher::Dispatcher;
use crate::agent::enricher::L1Enricher;
use crate::agent::graph_loop::{GraphLoop, GraphLoopConfig, LoopState};
use crate::agent::proposer::GraphProposer;
use crate::agent::repairer::LocalRepairer;
use crate::agent::reviewer::Reviewer;
use crate::agent::subagent::SubAgent;
use crate::agent::validator::{BashCheckValidator, PostExecutionValidator};
use crate::agent::verifier::Verifier;
use crate::graph::Graph;
use crate::model::{ModelConfig, ModelWithEvents};
use crate::tools::{BashTool, DangerousCommandDeny, EditFileTool, ReadFileTool, ToolContext, ToolRegistry, WebFetchTool, WebSearchTool, WriteFileTool};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;
use tokio_stream::StreamExt;

type AppState = State<Arc<WebState>>;

// --- Health ---

pub async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

// --- List runs ---

pub async fn list_runs(State(state): AppState) -> Result<Json<Vec<RunMetadata>>, ApiError> {
    let runs = state.runs.read().await;
    let mut out = Vec::new();
    for s in runs.values() {
        out.push(s.metadata().await);
    }
    // Sort by started_at descending (newest first). duration_ms is
    // monotonically increasing, so it acts as a proxy for start time.
    out.sort_by(|a, b| b.duration_ms.cmp(&a.duration_ms));
    Ok(Json(out))
}

// --- Create run ---

#[derive(Deserialize)]
pub struct CreateRunBody {
    pub task: String,
    /// Optional graph (L0 skeleton) to seed the run with. Used by the
    /// multi-turn chat flow so each new user message extends the
    /// previous run's graph instead of starting fresh.
    #[serde(default)]
    pub initial_graph: Option<super::events::InitialGraphDto>,
    /// Optional prior conversation transcript. Each entry is one
    /// message as the Proposer/SubAgent saw it. When present, the new
    /// turn's `Conversation` is seeded with these messages followed by
    /// a fresh `Task: ...` line — the agent remembers the chat.
    #[serde(default)]
    pub initial_transcript: Option<Vec<InitialMessage>>,
}

/// One entry in the prior conversation transcript. Mirrors the wire
/// shape of `RunEvent::Transcript` payloads from a previous turn.
#[derive(Deserialize)]
pub struct InitialMessage {
    pub role: String,
    pub content: String,
}

pub async fn create_run(
    State(state): State<Arc<WebState>>,
    Json(body): Json<CreateRunBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = uuid::Uuid::new_v4().to_string();
    // Read event_channel_capacity from disk so Settings changes take
    // effect on the next new run (state.config.engine is frozen at
    // startup; see drive_run for the longer comment).
    let capacity = EngineConfig::load().loop_tuning.event_channel_capacity;
    let session = Arc::new(RunSession::new(
        id.clone(),
        body.task.clone(),
        capacity,
    ));
    state.runs.write().await.insert(id.clone(), session.clone());

    // Spawn the run driver.
    let state_clone = state.clone();
    let id_clone = id.clone();
    let initial_graph = body.initial_graph;
    let initial_transcript = body.initial_transcript;
    tokio::spawn(async move {
        drive_run(state_clone, id_clone, initial_graph, initial_transcript).await;
    });

    Ok(Json(serde_json::json!({"id": id})))
}

// --- Get run ---

pub async fn get_run(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
) -> Result<Json<RunMetadata>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    Ok(Json(session.metadata().await))
}

// --- Cancel run ---

pub async fn cancel_run(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    session.cancel.cancel();
    Ok(Json(serde_json::json!({"cancelled": true})))
}

// --- Delete run ---

pub async fn delete_run(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let session = {
        let runs = state.runs.read().await;
        runs
            .get(&id)
            .map(|s| s.clone())
            .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?
    };
    // Cancel if still running, then remove.
    session.cancel.cancel();
    {
        let mut runs = state.runs.write().await;
        runs.remove(&id);
    }
    // Remove persisted data if any.
    let _ = state.persistence.delete_run(&id);
    tracing::info!(run_id = %id, "run deleted");
    Ok(Json(serde_json::json!({"deleted": true})))
}

// --- Clear all runs ---

pub async fn clear_runs(
    State(state): State<Arc<WebState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut runs = state.runs.write().await;
    let count = runs.len();
    for (id, session) in runs.iter() {
        session.cancel.cancel();
        let _ = state.persistence.delete_run(id);
    }
    runs.clear();
    tracing::info!(count, "all runs cleared");
    Ok(Json(serde_json::json!({"deleted": count})))
}

// --- SSE event stream ---

pub async fn run_events(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    let rx = session.event_tx.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
        .filter_map(|res| res.ok()) // skip lagged/inactive
        .map(|event: RunEvent| {
            let sse_event = Event::default()
                .event(event.event_name())
                .json_data(
                    event
                        .inner_json()
                        .unwrap_or(serde_json::Value::Null),
                )
                .unwrap_or_else(|_| Event::default().comment("serialization error"));
            Ok::<_, Infallible>(sse_event)
        });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

// --- Post answer (resume Paused) ---

#[derive(Deserialize)]
pub struct AnswerBody {
    pub answer: String,
}

pub async fn post_answer(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
    Json(body): Json<AnswerBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    if *session.status.read().await != RunStatus::Paused {
        return Err(ApiError::Conflict(format!("run {id} is not Paused")));
    }
    session.provide_answer(body.answer).await;
    Ok(Json(serde_json::json!({"accepted": true})))
}

// --- Post repair (resume GraphInvalid) ---

#[derive(Deserialize)]
pub struct RepairBody {
    pub graph: Graph,
}

pub async fn post_repair(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
    Json(body): Json<RepairBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    if *session.status.read().await != RunStatus::GraphInvalid {
        return Err(ApiError::Conflict(format!("run {id} is not GraphInvalid")));
    }
    session.provide_repair(body.graph).await;
    Ok(Json(serde_json::json!({"accepted": true})))
}

// --- Checkpoint endpoints ---

pub async fn list_checkpoints(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
) -> Result<Json<Vec<CheckpointMeta>>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    let store = session.checkpoints.lock().await;
    Ok(Json(store.list()))
}

pub async fn get_checkpoint(
    State(state): State<Arc<WebState>>,
    Path((id, idx)): Path<(RunId, usize)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    let store = session.checkpoints.lock().await;
    let cp = store
        .get(idx)
        .ok_or_else(|| ApiError::NotFound(format!("checkpoint {idx}")))?;
    Ok(Json(serde_json::json!({
        "index": cp.index,
        "round": cp.round,
        "phase": cp.phase.to_string(),
        "graph": cp.graph_snapshot,
        "transcript": cp.transcript,
    })))
}

#[derive(Deserialize)]
pub struct BranchBody {
    pub from_checkpoint: usize,
}

pub async fn create_branch(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
    Json(body): Json<BranchBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (parent_task, graph, transcript) = {
        let runs = state.runs.read().await;
        let parent = runs
            .get(&id)
            .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
        let store = parent.checkpoints.lock().await;
        let cp = store
            .get(body.from_checkpoint)
            .ok_or_else(|| ApiError::NotFound(format!("checkpoint {}", body.from_checkpoint)))?;
        (parent.task.clone(), cp.graph_snapshot.clone(), cp.transcript.clone())
    };

    let new_id = uuid::Uuid::new_v4().to_string();
    let capacity = EngineConfig::load().loop_tuning.event_channel_capacity;
    let new_session = Arc::new(RunSession::new(
        new_id.clone(),
        parent_task,
        capacity,
    ));
    state.runs.write().await.insert(new_id.clone(), new_session.clone());

    // Record branch relationship.
    {
        let runs = state.runs.read().await;
        if let Some(parent) = runs.get(&id) {
            let mut store = parent.checkpoints.lock().await;
            store.create_branch(body.from_checkpoint, new_id.clone());
        }
    }

    // Spawn new run driver with checkpoint as initial state.
    let state_clone = state.clone();
    let id_clone = new_id.clone();
    let initial_graph_dto = InitialGraphDto::from_graph(&graph);
    let initial_transcript: Vec<InitialMessage> = transcript
        .iter()
        .map(|m| InitialMessage {
            role: format!("{:?}", m.role).to_lowercase(),
            content: m.content.clone(),
        })
        .collect();

    tokio::spawn(async move {
        drive_run(
            state_clone,
            id_clone,
            Some(initial_graph_dto),
            Some(initial_transcript),
        )
        .await;
    });

    Ok(Json(serde_json::json!({"id": new_id})))
}

// --- Run driver ---

/// Loop engineering: when the main agent is stuck (ask_user), instead of
/// a generic "yes, proceed", spawn a "virtual user" model call that analyzes
/// the task + question + graph state and gives a concrete, actionable answer.
///
/// This is "大模型告诉大模型" — the model tells itself what to do next,
/// decomposing the task into the next concrete step.
async fn self_decompose_answer(
    model: &dyn crate::model::Model,
    task: &str,
    question: &str,
    graph_nodes: usize,
    graph_edges: usize,
) -> String {
    let prompt = format!(
        "\
你是任务拆解助手。主 Agent 正在执行一个自动化任务，但它卡住了。\
你的职责是分析当前情况，给出具体、可执行的方向来推动工作继续。\
**注意：不要提供 JSON 格式示例（你自己也不知道正确格式），只给方向性指导。**

## 当前任务
{task}

## Agent 的问题
{question}

## 当前图状态
{graph_nodes} 个节点, {graph_edges} 条边

## 具体指引
1. 如果图是空的（0节点）：告诉 Agent 立即建图。用 task 描述中的 A 和 D 作为锚点和目标，\
   加至少1个中间 Task 节点。所有节点 kind 必须是 Task，主链边 relation 用 LeadsTo，\
   A 节点必须设 immutable:true。
2. 如果 Agent 的 JSON 格式错误：告诉它\"简化你的输出，先生成一个最小可用的图（A→T1→D），\
   确保 add_nodes 数组有至少3个节点，add_edges 数组有至少2条边，每条边有 source 和 target 字段。\
   kind 固定为 Task，主链用 LeadsTo；中间步骤按需选 LeadsTo(流程)/DependsOn(依赖)/Contains(包含)。\"
3. 如果 Agent 问方向/做什么：从任务描述中直接提取 A 和 D，不要犹豫，直接选。
4. 在任何情况下：禁止建议 Agent 使用 ask_user 或 block — 这是无人值守循环。

用中文回答，控制在 200 字以内。不要反问，不要确认，直接给出决定。"
    );
    let req = crate::model::ModelRequest {
        messages: vec![crate::model::Message::user(prompt)],
        tools: vec![],
        temperature: 0.3,
        max_tokens: Some(512),
        stop: vec![],
    };
    match model.complete(req).await {
        Ok(resp) => {
            let answer = resp.content.trim().to_string();
            if answer.is_empty() {
                "立即建图执行。start=当前问题, deliverable=目标效果, 中间加 Task 步骤节点(用 LeadsTo 连)。不要犹豫，直接 propose_patch。".to_string()
            } else {
                answer
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "self_decompose: model call failed, using fallback");
            "继续执行。根据你的任务描述选择最明显的改进点，建 A→D 图，propose_patch 然后 SubAgent 执行。".to_string()
        }
    }
}

/// Trigger graceful shutdown so the external launcher (loop.ps1) restarts
/// us with the latest binary. Saves the heartbeat state first.
async fn heartbeat_trigger_shutdown(state: &Arc<WebState>) {
    let hb_active = {
        state.heartbeat.lock().await.as_ref()
            .map(|hb| hb.active).unwrap_or(false)
    };
    if !hb_active { return; }
    info!("heartbeat: round complete — triggering graceful shutdown for restart");
    let _ = state.shutdown_tx.send(true);
}

/// The actual agent loop. Spawned as a tokio task by `create_run`. Maps
/// each `LoopState` to events and broadcasts them on the session's
/// channel. Resolves `Paused` and `GraphInvalid` via the session's
/// `Notify` machinery.
pub async fn drive_run(
    state: Arc<WebState>,
    id: RunId,
    initial_graph: Option<super::events::InitialGraphDto>,
    initial_transcript: Option<Vec<InitialMessage>>,
) {
    let session = {
        let runs = state.runs.read().await;
        match runs.get(&id) {
            Some(s) => s.clone(),
            None => return,
        }
    };

    // Save run.json immediately so the run survives process kill.
    let _ = state.persistence.save_run_meta(&session.metadata().await);

    // Re-read the engine config from disk on every new run. The in-memory
    // `state.config.engine` is frozen at startup and is NOT updated by
    // POST /api/config (that handler only persists to disk and returns
    // the new value to the caller). This re-read is what makes the
    // "save drill-down" type changes actually take effect on the next
    // run. The disk read is ~one stat + small JSON parse; cheap.
    let engine = EngineConfig::load();

    // Build the GraphLoop. This mirrors what bin/agent_a does.
    let cfg = ModelConfig::from_engine_config(&engine.model).with_thinking(
        engine.loop_tuning.thinking_enabled,
        if engine.loop_tuning.reasoning_effort.is_empty() {
            None
        } else {
            Some(engine.loop_tuning.reasoning_effort.clone())
        },
    );
    let fast_model =
        ModelWithEvents::wrap(cfg.fast_model(), session.event_tx.clone(), "fast".into());
    let deep_model =
        ModelWithEvents::wrap(cfg.deep_model(), session.event_tx.clone(), "deep".into());

    // Tools.
    //
    // Pure-orchestrator wiring (per ARCHITECTURE §1a, the
    // main agent is a planner, subagents are its hands):
    //
    //   - `main_tool_registry` is what the main agent sees
    //     in its system prompt and what its `call_tool`
    //     would invoke. We make it empty so the model can
    //     only emit `explore` / `propose_patch` / `ask_user` /
    //     `block` / `ready_for_verify` — not `call_tool`
    //     (which would fail with "unknown tool" and waste a
    //     round). The system prompt's
    //     "(no direct tools available to you ...)"
    //     message tells the model this.
    //
    //   - `subagent_tool_registry` is what every explore
    //     subagent gets. It HAS bash / web_search / web_fetch
    //     because the subagent's job is to actually run
    //     commands and read files.
    //
    // The previous wiring used a single shared registry —
    // which is why the main agent could call bash directly
    // and got stuck in `ls` loops. Splitting the registries
    // is the root fix.
    // v2.7: read advanced tuning knobs from config so the UI Settings page
    // can override them without code changes.
    let advanced = engine.advanced.clone();
    let main_tool_registry = Arc::new(ToolRegistry::new());
    let subagent_tool_registry = Arc::new({
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(
            BashTool::new().with_default_timeout(std::time::Duration::from_millis(
                engine.advanced.bash_default_timeout_ms,
            )),
        ));
        reg.register(Arc::new(ReadFileTool::default()));
        reg.register(Arc::new(WriteFileTool::default()));
        reg.register(Arc::new(EditFileTool::default()));
        reg.register(Arc::new(WebSearchTool::new()));
        reg.register(Arc::new(WebFetchTool::new()));
        reg
    });
    // A tiny ToolContext just to keep `tool_cwd` canonical; the loop
    // also keeps a copy. We don't actually need to call it from here.
    let _tool_ctx = ToolContext::new(state.config.project_root.clone());

    // Shared PromptRegistry — used by both Proposer and SubAgent
    // for dynamic block injection (heartbeat, skill matching, etc.).
    let prompt_registry = Arc::new(
        crate::skills::prompt_registry::PromptRegistry::new(Some(&state.config.project_root))
    );

    // Proposer (with skills + dynamic prompt blocks).
    let mut proposer = GraphProposer::new(
        fast_model.clone(),
        main_tool_registry.clone(),
        Some(state.skills.clone()),
    )
    .with_prompt_registry(prompt_registry.clone())
    .with_max_tokens(advanced.proposer_default_max_tokens)
    .with_max_explore_items(advanced.explore_max_items_per_step)
    .with_max_explore_question_chars(advanced.explore_max_question_chars);

    // Optional advisor backend (consult_advisor). Wrapped so its output
    // streams to the UI under the "advisor" label.
    if let Some(advisor) = cfg.advisor_model() {
        let advisor =
            ModelWithEvents::wrap(advisor, session.event_tx.clone(), "advisor".into());
        proposer = proposer.with_advisor(advisor);
    }

    // Verifier, enricher, repairer.
    let verifier = Verifier::with_model(fast_model.clone());
    let loader: Arc<dyn crate::context::SourceLoader> =
        Arc::new(crate::context::NullSourceLoader);
    let enricher = L1Enricher::new(deep_model.clone(), loader.clone())
        .with_l2_char_cap(advanced.enricher_l2_char_cap)
        .with_neighbor_limit(advanced.enricher_neighbor_limit)
        .with_l0_only_confidence_cap(advanced.enricher_l0_only_confidence_cap);
    let repairer = LocalRepairer::new(deep_model.clone()).with_l1_enricher(enricher.clone());

    // Phase 3 — Decomposer + Dispatcher + SubAgent.
    //
    // SubAgent (used by the dispatcher in Task phase) gets
    // the FULL tool registry — it actually executes the
    // sub-tasks. The main agent gets the empty one (see
    // `main_tool_registry` above).
    let advanced = engine.advanced.clone();
    let decomposer = Decomposer::new(deep_model.clone())
        .with_max_tokens(advanced.decomposer_default_max_tokens);
    let subagent = Arc::new(
        SubAgent::new(fast_model.clone())
            .with_tools(subagent_tool_registry.clone())
            .with_policy(Arc::new(DangerousCommandDeny::new()))
            .with_prompt_registry(prompt_registry)
            .with_tool_cwd(state.config.project_root.clone())
            .with_tool_output_cap(6_000)
            .with_max_steps(advanced.subagent_max_steps)
            .with_max_tokens(advanced.subagent_default_max_tokens),
    );
    // 2 subagents in parallel = 1 main + 2 subagents total
    // (per [[project-concurrency-limits]]). Main runs single-threaded
    // as the orchestrator; the pool below caps subagent fan-out.
    let dispatcher = Dispatcher::new(subagent)
        .with_max_concurrent(engine.policy.max_concurrent_subagents);

    // Phase 4 — Reviewer + PostExecutionValidator.
    let reviewer = Reviewer::with_model(deep_model.clone());
    let validator: Arc<dyn PostExecutionValidator> =
        Arc::new(
            BashCheckValidator::cargo_check_for(&state.config.project_root)
                .with_timeout_ms(engine.advanced.validator_default_timeout_ms),
        );

    // v2: channel for cascade step events from the backtracker → WS clients.
    let (cascade_tx, mut cascade_rx) = tokio::sync::mpsc::unbounded_channel::<
        crate::agent::cascade::CascadeStep,
    >();

    // Async-safe: check heartbeat status once with .await (not block_on).
    // When heartbeat is active:
    //   - enforce strict A→D Task DAG pattern (graph_schema)
    //   - inject autonomous-mode system prompt blocks (is_heartbeat → no ask_user)
    let is_heartbeat_active = state.heartbeat.lock().await.as_ref()
        .map(|hb| hb.active).unwrap_or(false);

    let loop_cfg = GraphLoopConfig {
        max_rounds: engine.loop_tuning.max_rounds,
        max_repair_rounds: 3,
        tool_cwd: state.config.project_root.clone(),
        tool_output_cap: 8_000,
        tool_policy: Arc::new(DangerousCommandDeny::new()),
        auto_apply_skills: engine.loop_tuning.auto_apply_skills,
        stagnation_soft_hint: engine.loop_tuning.stagnation_soft_hint,
        stagnation_hard_hint: engine.loop_tuning.stagnation_hard_hint,
        stagnation_terminate: engine.loop_tuning.stagnation_terminate,
        stuck_soft_hint: engine.loop_tuning.stuck_soft_hint,
        stuck_hard_hint: engine.loop_tuning.stuck_hard_hint,
        stuck_terminate: engine.loop_tuning.stuck_terminate,
        tool_failure_warn_after: engine.loop_tuning.tool_failure_warn_after,
        tool_failure_halt_after: engine.loop_tuning.tool_failure_halt_after,
        force_search_after_filling_stall: engine.loop_tuning.force_search_after_filling_stall,
        convergence_stable_rounds: engine.loop_tuning.convergence_stable_rounds,
        max_drilldown_depth: engine.max_drilldown_depth as u32,
        sub_run_timeout_ms: Some(engine.sub_run_timeout_ms),
        skill_match_threshold: Some(engine.advanced.skill_match_threshold),
        skill_match_trigger_weight: Some(engine.advanced.skill_match_trigger_weight),
        skill_match_slug_weight: Some(engine.advanced.skill_match_slug_weight),
        cascade_max_expand_depth: Some(engine.advanced.cascade_max_expand_depth as u32),
        is_heartbeat: is_heartbeat_active,
        graph_schema: if is_heartbeat_active {
            Some(crate::agent::graph_loop::GraphSchema {
                allowed_node_kinds: vec![crate::graph::NodeKind::Task],
                required_edge_relation: Some(crate::graph::RelationType::LeadsTo),
                require_immutable_anchor: true,
                // Flexible: model decides the graph structure naturally.
                // A complex task may need A→B→C→D→E, a simple one just A→D.
                min_nodes: 1,
                min_edges: 0,
            })
        } else {
            None
        },
    };

    let mut gl = GraphLoop::new(
        session.task.clone(),
        proposer.clone(),
        verifier,
        Some(repairer),
        // Main agent's toolset: empty. Its only execution
        // path is `explore`, which dispatches subagents
        // (with the subagent_tool_registry below).
        main_tool_registry.clone(),
        loop_cfg,
    )
    .with_l1_enricher(enricher)
    // Override subagent toolset to give subagents the full
    // bash / web_search / web_fetch. Without this, the
    // graph-phase Explore subagent would inherit the
    // empty main registry and have no way to read files.
    .with_subagent_tools(subagent_tool_registry.clone())
    .with_decomposer(decomposer)
    .with_dispatcher(dispatcher)
    .with_subagent_loader(loader)
    .with_reviewer(reviewer)
    .with_validator(validator)
    // v2: cascade backtracking on sub-agent failure.
    .with_cascade(
        crate::agent::cascade::CascadeBacktracker::new(deep_model.clone())
            .with_step_sink(cascade_tx),
    )
    // v2.5: auto-match and apply stored skills.
    .with_skill_storage(state.skills.clone());

    if let Some(dto) = initial_graph {
        let g = dto.into_graph();
        *session.last_graph.write().await = Arc::new(g.clone());
        gl = gl.with_initial_graph(g);
    }

    // When continuing from a prior chat turn, seed the new
    // Conversation with the previous transcript. The system prompt is
    // rebuilt from the proposer (it depends on tools + skills, not on
    // prior messages), and the new "Task: ..." line is appended so
    // the very first Proposer turn sees both the history and the new
    // task. `ask_user` and other Proposer replies show up in their
    // proper roles.
    if let Some(prior) = initial_transcript.as_ref().filter(|t| !t.is_empty()) {
        let mut conv = proposer.make_conversation(&session.task);
        // Drop the auto-pinned "Task: <task>" first message — we'll
        // re-append the prior transcript first so the new task line
        // comes last and is the freshest signal.
        conv.messages.clear();
        use crate::model::{Message, Role};
        for m in prior {
            let role = match m.role.as_str() {
                "user" | "ask_user" => Role::User,
                "assistant" | "tool" => Role::Assistant,
                _ => Role::User,
            };
            conv.messages.push(Message {
                role,
                content: m.content.clone(),
            });
        }
        conv.messages.push(Message::user(format!("Task: {}", session.task)));
        gl = gl.with_initial_conversation(conv);
    }

    // Main loop.
    loop {
        if session.cancel.is_cancelled() {
            // Propagate the cancel signal to the GraphLoop so its
            // polling block can mark any pending sub-runs as Cancelled
            // and return LoopState::Error("parent cancelled") on the
            // next step. Without this, drill-down sub-runs would keep
            // running after the parent session was already torn down.
            gl.check_and_set_cancelled(true);
            *session.status.write().await = RunStatus::Cancelled;
            session.freeze_duration().await;
            let _ = state.persistence.save_run_meta(&session.metadata().await);
            session.emit(RunEvent::Done {
                final_result: serde_json::json!({"status": "cancelled"}),
            });
            return;
        }

        // Wire persistence on first access (lazy init).
        {
            let mut store = session.checkpoints.lock().await;
            if store.persistence.is_none() {
                store.persistence = Some(state.persistence.clone());
                store.run_id = session.id.clone();
            }
        }

        // Race the step against the cancel token so a stop click during a
        // long in-flight step (a model call can take up to ~180s) interrupts
        // immediately instead of waiting for the step to finish. Without this,
        // the cancel check at the top of the loop only fires between steps, so
        // "stop" appeared to do nothing for many seconds.
        let state_clone = tokio::select! {
            biased;
            _ = session.cancel.cancelled() => {
                // Mirror the cancel signal onto the GraphLoop so the
                // polling block can finalize pending sub-runs.
                gl.check_and_set_cancelled(true);
                *session.status.write().await = RunStatus::Cancelled;
                session.freeze_duration().await;
                let _ = state.persistence.save_run_meta(&session.metadata().await);
                session.emit(RunEvent::Done {
                    final_result: serde_json::json!({"status": "cancelled"}),
                });
                return;
            }
            s = gl.step() => s,
        };

        // v2: push checkpoint after each step.
        {
            use super::checkpoint::CheckpointPhase;
            let phase = match &state_clone {
                crate::agent::graph_loop::LoopState::Running => CheckpointPhase::Task,
                crate::agent::graph_loop::LoopState::Paused { .. } => CheckpointPhase::Graph,
                _ => CheckpointPhase::Review,
            };
            let mut store = session.checkpoints.lock().await;
            store.push(gl.round, phase, &gl.graph, &gl.conversation.messages);
            let cp_count = store.len();
            drop(store);
            if cp_count % 5 == 0 {
                // Emit a checkpoint-created event every 5 steps (not every step — too noisy).
                session.emit(RunEvent::CheckpointCreated {
                    index: cp_count - 1,
                    round: gl.round,
                    phase: phase.to_string(),
                    node_count: gl.graph.node_count(),
                    edge_count: gl.graph.edge_count(),
                });
            }
        }

        // v2: drain any pending cascade steps.
        while let Ok(step) = cascade_rx.try_recv() {
            session.emit(RunEvent::CascadeStep {
                changed_node: step.changed_node,
                predecessor: step.predecessor,
                depth: step.depth,
                verdict: step.verdict,
                rationale: step.rationale,
            });
        }

        // Emit the incremental diff BEFORE the snapshot — patch diffs
        // against the previous last_graph, then the snapshot updates it.
        session.emit_graph_patch(&gl.graph).await;
        session.emit_graph_snapshot(&gl.graph).await;
        // Status update — fires after every step so the UI can show
        // cumulative token cost + current phase. Cheap (one small
        // event) and the frontend throttles render anyway.
        let (status_phase, status_msg) = match &state_clone {
            crate::agent::graph_loop::LoopState::Running => ("graph", "thinking...".into()),
            crate::agent::graph_loop::LoopState::Paused { .. } => ("paused", "waiting for user".into()),
            crate::agent::graph_loop::LoopState::GraphInvalid { .. } => {
                ("graph_invalid", "graph has issues, repairing".into())
            }
            crate::agent::graph_loop::LoopState::Done(_) => ("done", "complete".into()),
            crate::agent::graph_loop::LoopState::Error(_) => ("error", "loop error".into()),
            crate::agent::graph_loop::LoopState::TaskFailed { .. } => ("task_failed", "sub-task failed".into()),
        };
        // Sync tokens to session for usage stats.
        *session.tokens_used.lock().await = gl.tokens_used;

        session.emit(RunEvent::Status {
            phase: status_phase.into(),
            message: status_msg,
            tokens_used: gl.tokens_used,
        });
        let kind = state_kind(&state_clone);
        session.emit(RunEvent::LoopState {
            kind: kind.to_string(),
            payload: loop_state_payload(&state_clone),
        });

        // Capture tool call info before consuming the steps.
        let tool_info: Option<(String, String)> = gl.last_step.as_ref().and_then(|s| {
            if let crate::agent::proposer::ProposerStep::CallTool { tool, args, .. } = s {
                Some((tool.clone(), serde_json::to_string(args).unwrap_or_default()))
            } else { None }
        });

        // Surface each Proposer step as a transcript event.
        if let Some(step) = gl.last_step.take() {
            for (role, content) in step_transcripts(&step) {
                session.emit(RunEvent::Transcript { role, content });
            }
        }

        // Emit tool result + structured ToolUse event for the trace.
        if let Some((tool, summary)) = gl.last_tool_result.take() {
            if let Some((tool_name, args_str)) = tool_info {
                session.emit(RunEvent::ToolUse {
                    component: "main_agent".into(),
                    tool: tool_name,
                    args: args_str,
                    output: summary.clone(),
                    exit_code: None,
                    duration_ms: 0,
                });
            }
            session.emit(RunEvent::Transcript {
                role: "tool_result".into(),
                content: format!("📥 {tool} → {summary}"),
            });
        }

        match state_clone {
            LoopState::Paused { question, .. } => {
                *session.status.write().await = RunStatus::Paused;
                // Persist immediately so the list-runs endpoint (which
                // reads state.runs) and the disk (run.json) don't
                // desync — the user saw the sidebar say "Running" but
                // the details panel say "Paused" because the previous
                // save (at run start) still had Running and the pause
                // never hit disk. (See task 01525e4c for the bug report.)
                let _ = state.persistence.save_run_meta(&session.metadata().await);
                session.emit(RunEvent::Transcript {
                    role: "ask_user".into(),
                    content: question.clone(),
                });
                // Heartbeat: loop engineering — instead of a generic
                // "yes, proceed", spawn a "virtual user" model call that
                // analyzes the task + question + graph and gives concrete,
                // actionable direction. This is "大模型告诉大模型".
                let is_heartbeat = state.heartbeat.lock().await.as_ref()
                    .map(|hb| hb.active).unwrap_or(false);
                let answer = if is_heartbeat {
                    info!("heartbeat: self-decompose via virtual user model call");
                    *session.status.write().await = RunStatus::Running;
                    // Persist the resume too — the user could refresh
                    // the page mid-self-decompose and we'd want the
                    // sidebar to show "Running" again immediately.
                    let _ = state.persistence.save_run_meta(&session.metadata().await);
                    let answer = self_decompose_answer(
                        &*fast_model,
                        &session.task,
                        &question,
                        gl.graph.node_count(),
                        gl.graph.edge_count(),
                    ).await;
                    session.emit(RunEvent::Transcript {
                        role: "user".into(),
                        content: answer.clone(),
                    });
                    answer
                } else {
                    session.await_answer().await
                };
                gl.resume(answer);
            }
            LoopState::GraphInvalid { source, errors, snapshot } => {
                *session.status.write().await = RunStatus::GraphInvalid;
                // Persist the GraphInvalid status so reload / refresh
                // shows the right indicator. Without this, the disk
                // would still say "Running" while in-memory says
                // "GraphInvalid" (same desync as Paused).
                let _ = state.persistence.save_run_meta(&session.metadata().await);
                let payload = serde_json::json!({
                    "source": format!("{source:?}"),
                    "error_count": errors.len(),
                    "snapshot": {
                        "nodes": snapshot.node_count(),
                        "edges": snapshot.edge_count(),
                    }
                });
                session.emit(RunEvent::LoopState {
                    kind: "GraphInvalid".into(),
                    payload,
                });
                // v1: receive a repaired graph from the user.
                // Heartbeat: auto-repair with the current graph snapshot.
                let is_heartbeat = state.heartbeat.lock().await.as_ref()
                    .map(|hb| hb.active).unwrap_or(false);
                let repaired = if is_heartbeat {
                    info!("heartbeat: auto-repairing GraphInvalid");
                    snapshot.clone()
                } else {
                    session.await_repair().await
                };
                gl.resume_with_repaired_graph(repaired);
            }
            LoopState::Done(final_result) => {
                *session.status.write().await = RunStatus::Done;
                session.freeze_duration().await;
                // Persist run metadata so it survives restarts.
                let _ = state.persistence.save_run_meta(&session.metadata().await);
                session.emit(RunEvent::Done {
                    final_result: serde_json::to_value(&final_result).unwrap_or(serde_json::Value::Null),
                });

                // Fire `capture_skill` when the Reviewer said Pass.
                // The web gateway awaits the JoinHandle (unlike the CLI
                // which discards it) so we can emit a SkillCaptured
                // event with the actual slug + trigger for the UI.
                let review_passed = final_result
                    .review_result
                    .as_ref()
                    .map(|r| r.passed)
                    .unwrap_or(false);
                if review_passed && !final_result.graph.nodes.is_empty() {
                    let proposer_model = gl.proposer.model.clone();
                    let review_json = serde_json::to_value(&final_result.review_result)
                        .unwrap_or(serde_json::Value::Null);
                    let task_str = session.task.clone();
                    let graph_clone = final_result.graph.clone();
                    let local_storage = state
                        .skills
                        .local_root()
                        .map(|p| {
                            std::sync::Arc::new(crate::skills::storage::LocalSkillStorage::new(p))
                        });
                    if let Some(storage) = local_storage {
                        let handle = crate::skills::capture::capture_skill(
                            graph_clone,
                            review_json,
                            task_str,
                            None,
                            proposer_model,
                            storage,
                        );
                        let session_clone = session.clone();
                        tokio::spawn(async move {
                            match handle.await {
                                Ok(Ok(skill_ref)) => {
                                    *session_clone.captured_skill.write().await =
                                        Some(skill_ref.clone());
                                    session_clone.emit(RunEvent::SkillCaptured {
                                        slug: skill_ref.slug,
                                        trigger: skill_ref.trigger,
                                    });
                                }
                                Ok(Err(e)) => {
                                    tracing::warn!("skill capture failed: {e}");
                                    session_clone.emit(RunEvent::Transcript {
                                        role: "assistant".into(),
                                        content: format!("(skill capture failed: {e})"),
                                    });
                                }
                                Err(e) => {
                                    tracing::warn!("skill capture join failed: {e}");
                                }
                            }
                        });
                    }
                }
                // Heartbeat: round completed successfully.
                let do_spawn = {
                    let mut hb_guard = state.heartbeat.lock().await;
                    if let Some(ref mut hb) = *hb_guard {
                        if hb.active { hb.round_complete() } else { false }
                    } else { false }
                };
                if do_spawn { heartbeat_trigger_shutdown(&state).await; }
                return;
            }
            LoopState::Error(msg) => {
                *session.status.write().await = RunStatus::Error(msg.clone());
                session.freeze_duration().await;
                let _ = state.persistence.save_run_meta(&session.metadata().await);
                session.emit(RunEvent::Error { message: msg });
                // Heartbeat: error counts as learning — advance to next round.
                let do_spawn = {
                    let mut hb_guard = state.heartbeat.lock().await;
                    if let Some(ref mut hb) = *hb_guard {
                        if hb.active { hb.round_complete() } else { false }
                    } else { false }
                };
                if do_spawn { heartbeat_trigger_shutdown(&state).await; }
                return;
            }
            LoopState::TaskFailed { failures } => {
                *session.status.write().await =
                    RunStatus::Error(format!("task failed: {failures:?}"));
                session.freeze_duration().await;
                let _ = state.persistence.save_run_meta(&session.metadata().await);
                session.emit(RunEvent::Error {
                    message: format!("task failed: {failures:?}"),
                });
                let do_spawn = {
                    let mut hb_guard = state.heartbeat.lock().await;
                    if let Some(ref mut hb) = *hb_guard {
                        if hb.active { hb.round_complete() } else { false }
                    } else { false }
                };
                if do_spawn { heartbeat_trigger_shutdown(&state).await; }
                return;
            }
            LoopState::Running => {
                // Continue looping.
            }
        }
    }
}

fn state_kind(s: &LoopState) -> &'static str {
    match s {
        LoopState::Running => "Running",
        LoopState::Paused { .. } => "Paused",
        LoopState::GraphInvalid { .. } => "GraphInvalid",
        LoopState::TaskFailed { .. } => "TaskFailed",
        LoopState::Done(_) => "Done",
        LoopState::Error(_) => "Error",
    }
}

/// Translate a Proposer step into one or two short, human-readable
/// transcript lines. Returns `None` only when there is literally
/// nothing useful to show.
///
/// We emit two lines per step when the rationale is non-empty:
///
/// 1. The model's own voice to the user (the `rationale` field,
///    which the prompt encourages the model to use as a free-form
///    reply — particularly for non-graph questions like "what model
///    are you?").
/// 2. The structured action summary (📝 patch, 🤔 question, 🔧 tool,
///    ✅ verify) so the user can still see what the graph side
///    is doing at a glance.
fn step_transcripts(
    step: &crate::agent::proposer::ProposerStep,
) -> smallvec::SmallVec<[(String, String); 2]> {
    use crate::agent::proposer::ProposerStep;
    use smallvec::SmallVec;
    let mut out: SmallVec<[(String, String); 2]> = SmallVec::new();

    // 1. The rationale, if the model wrote one. This is the primary
    //    "voice" of the assistant — the prompt encourages putting any
    //    answer to a non-graph question here.
    let rationale = match step {
        ProposerStep::AskUser { rationale, .. }
        | ProposerStep::CallTool { rationale, .. }
        | ProposerStep::ProposePatch { rationale, .. }
        | ProposerStep::ReadyForVerify { rationale, .. }
        | ProposerStep::Block { rationale, .. }
        | ProposerStep::Explore { rationale, .. }
        | ProposerStep::ConsultAdvisor { rationale, .. } => rationale.trim().to_string(),
    };
    if !rationale.is_empty() {
        out.push(("assistant".into(), rationale));
    }

    // 2. The structured action summary.
    let action: Option<(String, String)> = match step {
        ProposerStep::AskUser { question, .. } => {
            if question.trim().is_empty() {
                None
            } else {
                Some(("ask_user".into(), format!("🤔 {question}")))
            }
        }
        ProposerStep::Block { reason, needed_from_user, .. } => {
            // Surface the blocker to the chat so the user can
            // immediately see what the model is waiting on.
            if reason.trim().is_empty() {
                None
            } else if needed_from_user.trim().is_empty() {
                Some(("block".into(), format!("🚧 {reason}")))
            } else {
                Some((
                    "block".into(),
                    format!("🚧 {reason} — {needed_from_user}"),
                ))
            }
        }
        ProposerStep::Explore { items, .. } => {
            // Show in the chat that the model dispatched one or
            // more subagents. The actual subagent results will
            // arrive as a separate transcript event when they
            // return.
            let summary = match items.len() {
                1 => {
                    let item = &items[0];
                    format!("🔍 explore: {} (scope: {})", item.question, item.scope)
                }
                n => format!("🔍 explore batch: {} items dispatched in parallel", n),
            };
            Some(("explore".into(), summary))
        }
        ProposerStep::ProposePatch { patch, .. } => {
            let n = patch.add_nodes.len();
            let e = patch.add_edges.len();
            let r = patch.remove_node_ids.len();
            let x = patch.remove_edge_indices.len();
            let l = patch.set_l1.len();
            let mut parts: Vec<String> = Vec::new();
            if n > 0 { parts.push(format!("+{n} node{}", if n == 1 { "" } else { "s" })); }
            if e > 0 { parts.push(format!("+{e} edge{}", if e == 1 { "" } else { "s" })); }
            if r > 0 { parts.push(format!("-{r} node id{}", if r == 1 { "" } else { "s" })); }
            if x > 0 { parts.push(format!("-{x} edge index{}", if x == 1 { "" } else { "es" })); }
            if l > 0 { parts.push(format!("{l} L1 update{}", if l == 1 { "" } else { "s" })); }
            if parts.is_empty() {
                Some(("assistant".into(), "📝 proposing empty patch (no-op)".into()))
            } else {
                let body = parts.join(", ");
                let reason_text = patch.reason.trim().to_string();
                let reason = if reason_text.is_empty() {
                    body.clone()
                } else {
                    format!("{body} — {reason_text}")
                };
                Some(("assistant".into(), format!("📝 {reason}")))
            }
        }
        ProposerStep::CallTool { tool, args, .. } => {
            let arg_summary = args.as_object().map(|obj| {
                obj.iter()
                    .take(3)
                    .map(|(k, v)| {
                        let v = match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        let one_line: String = v.chars().take(40).collect();
                        format!("{k}={one_line}")
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            let suffix = arg_summary
                .map(|a| if a.is_empty() { String::new() } else { format!(" {a}") })
                .unwrap_or_default();
            Some(("tool".into(), format!("🔧 {tool}{suffix}")))
        }
        ProposerStep::ReadyForVerify { .. } => {
            Some(("assistant".into(), "✅ ready for verification".into()))
        }
        ProposerStep::ConsultAdvisor { question, .. } => {
            if question.trim().is_empty() {
                None
            } else {
                Some(("consult_advisor".into(), format!("💬 consulting advisor: {question}")))
            }
        }
    };
    if let Some((role, content)) = action {
        out.push((role, content));
    }

    out
}

// --- Usage endpoint ---

#[derive(serde::Serialize)]
pub struct UsageStats {
    total_tokens: u64,
    total_runs: usize,
    model_breakdown: std::collections::HashMap<String, ModelUsage>,
    runs: Vec<RunUsage>,
    tool_stats: std::collections::HashMap<String, crate::tools::ToolStats>,
}

#[derive(serde::Serialize)]
pub struct ModelUsage {
    calls: u64,
    tokens: u64,
}

#[derive(serde::Serialize)]
pub struct RunUsage {
    id: String,
    task: String,
    status: String,
    tokens: u64,
    duration_ms: u64,
}

pub async fn get_usage(
    State(state): State<Arc<WebState>>,
) -> Result<Json<UsageStats>, ApiError> {
    let runs = state.runs.read().await;
    let mut total_tokens: u64 = 0;
    let mut model_breakdown = std::collections::HashMap::new();
    let mut run_list = Vec::new();

    for s in runs.values() {
        let meta = s.metadata().await;
        let tokens = meta.tokens_used;
        total_tokens += tokens;
        run_list.push(RunUsage {
            id: meta.id.clone(),
            task: meta.task.clone(),
            status: format!("{:?}", meta.status),
            tokens,
            duration_ms: meta.duration_ms,
        });
        // Collect model usage from config (what's currently configured).
        let engine_cfg = EngineConfig::load();
        let model_key = format!("fast:{} deep:{}",
            engine_cfg.model.fast_model,
            engine_cfg.model.deep_model,
        );
        model_breakdown.entry(model_key).or_insert(ModelUsage { calls: 0, tokens: 0 }).tokens += tokens;
    }

    Ok(Json(UsageStats {
        total_tokens,
        total_runs: runs.len(),
        model_breakdown,
        runs: run_list,
        tool_stats: std::collections::HashMap::new(),
    }))
}

// --- Task 11: drill-down sub-graph API endpoints ---

/// GET /api/runs/:id/sub-runs — list every [`SubRunLink`] accumulated
/// across this run's checkpoints (links from complex nodes to forked
/// sub-runs). The frontend uses this to render the drill-down graph.
///
/// Reads every `checkpoints/*.json` file under the run dir and
/// concatenates their `sub_run_links` lists. 404 if the checkpoints
/// directory does not exist (i.e. the run has never persisted a
/// checkpoint — typical for unknown run ids).
pub async fn get_sub_runs(
    State(state): AppState,
    Path(id): Path<String>,
) -> Result<Json<Vec<SubRunLink>>, StatusCode> {
    let ckpt_dir = state.persistence.data_dir.join(&id).join("checkpoints");
    let entries = match std::fs::read_dir(&ckpt_dir) {
        Ok(e) => e,
        Err(_) => return Err(StatusCode::NOT_FOUND),
    };
    let mut all_links: Vec<SubRunLink> = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let s = match std::fs::read_to_string(entry.path()) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Ok(ckpt) = serde_json::from_str::<Checkpoint>(&s) {
            all_links.extend(ckpt.sub_run_links);
        }
    }
    Ok(Json(all_links))
}

/// GET /api/runs/:id/parent — return this run's parent run id, if any.
/// For top-level runs the field is missing/null. 404 if `run.json` does
/// not exist (i.e. the run was never persisted — typical for unknown ids).
pub async fn get_parent(
    State(state): AppState,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let run_json = state.persistence.data_dir.join(&id).join("run.json");
    let s = match std::fs::read_to_string(&run_json) {
        Ok(s) => s,
        Err(_) => return Err(StatusCode::NOT_FOUND),
    };
    let v: serde_json::Value =
        serde_json::from_str(&s).map_err(|_| StatusCode::NOT_FOUND)?;
    let parent = v
        .get("parent_run_id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Ok(Json(serde_json::json!({ "parent_run_id": parent })))
}

#[cfg(test)]
mod step_transcripts_tests {
    use super::step_transcripts;
    use crate::agent::proposer::ProposerStep;
    use crate::graph::{Graph, GraphPatch, Node};

    fn first_action(s: &[(String, String)]) -> Option<&(String, String)> {
        s.iter().find(|(r, _)| r == "assistant" || r == "tool" || r == "ask_user")
    }
    fn rationale(s: &[(String, String)]) -> Option<&str> {
        s.iter()
            .find(|(r, c)| r == "assistant" && (c.starts_with("📝") || c.starts_with("🤔") || c.starts_with("✅") || c.contains("📚") || c.contains("(skill capture failed")))
            .map(|(_, c)| c.as_str())
    }

    #[test]
    fn ask_user_emits_rationale_then_question() {
        // Order: model voice first (rationale), then the ask_user question.
        let step = ProposerStep::AskUser {
            question: "what's the deadline?".into(),
            options: vec![],
            rationale: "I need a date to plan around".into(),
        };
        let lines = step_transcripts(&step);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, "assistant");
        assert_eq!(lines[0].1, "I need a date to plan around");
        assert_eq!(lines[1].0, "ask_user");
        assert!(lines[1].1.contains("what's the deadline?"));
    }

    #[test]
    fn ask_user_with_empty_rationale_skips_voice_line() {
        // When the model writes no rationale, only the question shows.
        let step = ProposerStep::AskUser {
            question: "q".into(),
            options: vec![],
            rationale: "".into(),
        };
        let lines = step_transcripts(&step);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, "ask_user");
    }

    #[test]
    fn propose_patch_with_rationale_emits_both_lines() {
        // The rationale is the model's reply to the user; the action
        // line is the structured patch summary.
        let step = ProposerStep::ProposePatch {
            patch: GraphPatch::default(),
            rationale: "I'll start a graph for your question".into(),
        };
        let lines = step_transcripts(&step);
        assert_eq!(lines[0].0, "assistant");
        assert_eq!(lines[0].1, "I'll start a graph for your question");
        assert_eq!(lines[1].0, "assistant");
        assert!(lines[1].1.contains("no-op"));
    }

    #[test]
    fn propose_patch_summary_includes_node_edge_remove_and_reason() {
        let mut patch = GraphPatch::default();
        patch.add_nodes.push(Node::file("a", "A"));
        patch.add_nodes.push(Node::file("b", "B"));
        patch.add_nodes.push(Node::file("c", "C"));
        patch.add_edges.push(crate::graph::Edge::new(
            "a", "b", crate::graph::RelationType::Imports, 0.9, "",
        ));
        patch.remove_node_ids.push("z".into());
        patch.reason = "found missing module".into();
        let step = ProposerStep::ProposePatch {
            patch,
            rationale: "".into(),
        };
        let lines = step_transcripts(&step);
        // No rationale → only the action line.
        assert_eq!(lines.len(), 1);
        let (_, content) = &lines[0];
        assert!(content.contains("+3 nodes"));
        assert!(content.contains("+1 edge"));
        assert!(content.contains("-1 node id"));
        assert!(content.contains("found missing module"));
    }

    #[test]
    fn call_tool_emits_rationale_then_action() {
        let step = ProposerStep::CallTool {
            tool: "bash".into(),
            args: serde_json::json!({"command": "ls -la", "description": "list files"}),
            rationale: "see what's there".into(),
        };
        let lines = step_transcripts(&step);
        assert_eq!(lines[0].0, "assistant");
        assert_eq!(lines[0].1, "see what's there");
        assert_eq!(lines[1].0, "tool");
        assert!(lines[1].1.contains("bash"));
        assert!(lines[1].1.contains("command=ls -la"));
    }

    #[test]
    fn ready_for_verify_emits_rationale() {
        let step = ProposerStep::ReadyForVerify {
            rationale: "the graph covers it now".into(),
        };
        let lines = step_transcripts(&step);
        // Both rationale and the action summary fire.
        assert_eq!(lines[0].1, "the graph covers it now");
        assert!(lines[1].1.contains("ready for verification"));
    }

    #[test]
    fn graph_compiles_with_patch() {
        let _g = Graph::new();
    }

    // Keep the unused helper referenced so it doesn't warn.
    #[allow(dead_code)]
    fn _silence(_s: &[(String, String)]) {
        let _ = first_action(_s);
        let _ = rationale(_s);
    }
}

fn loop_state_payload(s: &LoopState) -> serde_json::Value {
    match s {
        LoopState::Running => serde_json::json!({}),
        LoopState::Paused { question, options, rationale } => {
            serde_json::json!({"question": question, "options": options, "rationale": rationale})
        }
        LoopState::GraphInvalid { source, errors, snapshot } => serde_json::json!({
            "source": format!("{source:?}"),
            "error_count": errors.len(),
            "snapshot": {
                "nodes": snapshot.node_count(),
                "edges": snapshot.edge_count(),
            }
        }),
        LoopState::TaskFailed { failures } => {
            serde_json::json!({"failure_count": failures.len()})
        }
        LoopState::Done(r) => serde_json::json!({
            "rounds": r.rounds,
            "graph_nodes": r.graph.node_count(),
            "graph_edges": r.graph.edge_count(),
        }),
        LoopState::Error(msg) => serde_json::json!({"message": msg}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use crate::skills::storage::LocalSkillStorage;

    fn make_state() -> Arc<WebState> {
        let dir = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let cfg = super::super::state::WebConfig {
            bind_addr: "0.0.0.0:0".to_string(),
            static_dir: String::new(),
            project_root: dir.path().to_path_buf(),
            engine: super::super::state::EngineConfig::default(),
        };
        Arc::new(super::super::WebState::new(local, cfg))
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        let resp = health().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn create_run_returns_id() {
        let state = make_state();
        let resp = create_run(
            State(state.clone()),
            Json(CreateRunBody {
                task: "test".into(),
                initial_graph: None,
                initial_transcript: None,
            }),
        )
        .await
        .unwrap()
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn get_unknown_run_returns_404() {
        let state = make_state();
        let err = get_run(State(state), Path("nope".into())).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cancel_unknown_run_returns_404() {
        let state = make_state();
        let err = cancel_run(State(state), Path("nope".into())).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn post_answer_to_running_run_returns_409() {
        let state = make_state();
        let id = uuid::Uuid::new_v4().to_string();
        let session = Arc::new(RunSession::new(id.clone(), "t".into(), 256));
        state.runs.write().await.insert(id.clone(), session);
        let err = post_answer(
            State(state),
            Path(id),
            Json(AnswerBody {
                answer: "x".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::CONFLICT);
    }

    // ---- Task 11: drill-down API endpoints ----
    //
    // Builds an axum Router rooted at the supplied tempdir, so the
    // /api/runs/:id/sub-runs and /api/runs/:id/parent handlers can
    // read directly from `<data_dir>/<id>/...`.

    use axum::body::Body;
    use axum::http::{Request, StatusCode as HttpStatus};
    use axum::Router;
    use tower::ServiceExt;

    fn test_app_with_data_root(data_dir: &std::path::Path) -> Router {
        let local = Arc::new(LocalSkillStorage::new(data_dir.to_path_buf()));
        let cfg = super::super::state::WebConfig {
            bind_addr: "0.0.0.0:0".to_string(),
            static_dir: String::new(),
            project_root: data_dir.to_path_buf(),
            engine: super::super::state::EngineConfig::default(),
        };
        // Build a real WebState then override its persistence to root at
        // the exact tempdir (bypassing the `<root>/data/runs` layout).
        let state = super::super::WebState {
            persistence: super::super::persistence::RunPersistence::with_data_dir(
                data_dir.to_path_buf(),
            ),
            ..super::super::WebState::new(local, cfg)
        };
        super::super::router(state, "")
    }

    #[tokio::test]
    async fn get_sub_runs_returns_200_with_links() {
        use crate::graph::NodeId;
        use crate::web::checkpoint::{Checkpoint, CheckpointPhase, SubRunLink};

        let tmp = tempfile::tempdir().unwrap();
        let run_id = "parent-test-1";
        let sub_id = "sub-test-1";
        let ckpt_dir = tmp.path().join(run_id).join("checkpoints");
        std::fs::create_dir_all(&ckpt_dir).unwrap();
        let ckpt = Checkpoint {
            index: 1,
            round: 1,
            phase: CheckpointPhase::Task,
            graph_snapshot: Graph::new(),
            transcript: vec![],
            sub_run_links: vec![SubRunLink {
                node_id: NodeId::from("design-modules"),
                sub_run_id: sub_id.into(),
                sub_status: "running".into(),
                created_at: 1000,
            }],
        };
        std::fs::write(
            ckpt_dir.join("0001.json"),
            serde_json::to_string(&ckpt).unwrap(),
        )
        .unwrap();

        let app = test_app_with_data_root(tmp.path());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/runs/{run_id}/sub-runs"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::OK);
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        let arr = body.as_array().expect("expected array");
        assert!(arr.len() >= 1);
        assert_eq!(arr[0]["sub_run_id"], sub_id);
        assert_eq!(arr[0]["sub_status"], "running");
        assert_eq!(arr[0]["node_id"], "design-modules");
    }

    #[tokio::test]
    async fn get_parent_returns_200_with_parent_id() {
        let tmp = tempfile::tempdir().unwrap();
        let sub_id = "sub-test-1";
        let run_dir = tmp.path().join(sub_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(
            run_dir.join("run.json"),
            r#"{"status":"Done","parent_run_id":"parent-1"}"#,
        )
        .unwrap();

        let app = test_app_with_data_root(tmp.path());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/runs/{sub_id}/parent"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::OK);
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["parent_run_id"], "parent-1");
    }

    #[tokio::test]
    async fn get_sub_runs_returns_404_for_unknown_run() {
        let app = test_app_with_data_root(tempfile::tempdir().unwrap().path());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/runs/nonexistent/sub-runs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::NOT_FOUND);
    }
}
