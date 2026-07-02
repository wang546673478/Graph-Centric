//! SubTask + SubAgent — the unit of work executed in the Task phase.
//!
//! A [`SubTask`] is the runtime form of a task-graph node: the description,
//! the world-graph nodes it operates on, and what capabilities it needs.
//! A [`SubAgent`] takes one SubTask, builds local context via
//! [`ContextBuilder`], and runs a **tool-calling ReAct loop** until the
//! model emits a final answer or `max_steps` is reached.
//!
//! ## Tool-calling protocol
//!
//! Each turn the model emits exactly one JSON action as its entire response:
//!
//! ```json
//! {"action": "use_tool", "tool": "<name>", "args": {...}, "thinking": "<why>"}
//! ```
//! or
//! ```json
//! {"action": "final_answer", "answer": "<result>", "thinking": "<why complete>"}
//! ```
//!
//! When the model calls a tool the SubAgent dispatches via
//! [`ToolRegistry`](crate::tools::ToolRegistry) under the configured
//! [`Policy`](crate::tools::Policy), feeds the result back as a user
//! message, and asks for the next action.
//!
//! If the model produces plain text (no parseable JSON action), the
//! SubAgent treats the entire response as a `final_answer` — this is the
//! graceful-degradation path for models that don't respect the JSON
//! protocol.
//!
//! ## Phase 4 (not yet)
//!
//! - Nested GraphLoop inside the sub-agent so it can build its own local
//!   graph
//! - Graph-error detection that bubbles a `GraphError` back to the parent
//! - OpenAI native function-calling protocol (with `tools` + `tool_calls`
//!   on Message) — for now the JSON-action protocol is portable across
//!   any model that follows instructions.

use super::contract::{CheckContract, ContractOutcome};
use super::graph_loop::{GraphError, L0ErrorType};
use super::proposer::extract_json_block;
use crate::context::{ContextBuilder, SourceLoader};
use crate::domain::TaskNeeds;
use crate::error::Result;
#[cfg(test)]
use crate::error::HarnessError;
use crate::graph::{Graph, Node, NodeId};
use crate::model::{Message, Model, ModelRequest, Role};
use crate::tools::{DangerousCommandDeny, Policy, ScopeGuard, ToolContext, ToolRegistry};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// SubTask
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubTask {
    pub id: NodeId,
    pub description: String,
    /// World-graph node ids this task focuses on. Used to extract the
    /// local subgraph that becomes the sub-agent's context.
    #[serde(default)]
    pub involved_nodes: Vec<NodeId>,
    #[serde(default)]
    pub needs: TaskNeeds,
    /// Pre-dispatch verification predicate.
    #[serde(default)]
    pub contract: CheckContract,
    /// Optional role-specific prompt injected into the sub-agent's system
    /// prompt. Use for domain-specific instructions (e.g., "code editor",
    /// "explorer", "security auditor"). When empty, uses default prompt.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role_prompt: String,
}

impl SubTask {
    pub fn from_task_node(node: &Node) -> Result<Self> {
        let involved_nodes: Vec<NodeId> = node
            .metadata
            .get("involved_nodes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.as_str().map(NodeId::from))
                    .collect()
            })
            .unwrap_or_default();
        let needs: TaskNeeds = node
            .metadata
            .get("needs")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let contract: CheckContract = node
            .metadata
            .get("contract")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let role_prompt = node
            .metadata
            .get("role_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(Self {
            id: node.id.clone(),
            description: node.summary.clone(),
            involved_nodes,
            needs,
            contract,
            role_prompt,
        })
    }

    pub fn to_task_node(&self) -> Node {
        let mut node = Node::task(self.id.as_str(), &self.description);
        let inv_ids: Vec<&str> = self.involved_nodes.iter().map(NodeId::as_str).collect();
        node = node.with_metadata("involved_nodes", serde_json::json!(inv_ids));
        node = node.with_metadata(
            "needs",
            serde_json::to_value(&self.needs).unwrap_or(serde_json::json!({})),
        );
        // Serialize the contract only when non-trivial; `None` is the
        // default and produces no metadata entry. This keeps the wire
        // format compact for the common case.
        if !matches!(self.contract, CheckContract::None) {
            node = node.with_metadata(
                "contract",
                serde_json::to_value(&self.contract).unwrap_or(serde_json::json!(null)),
            );
        }
        if !self.role_prompt.is_empty() {
            node = node.with_metadata("role_prompt", serde_json::json!(self.role_prompt));
        }
        node
    }
}

// ---------------------------------------------------------------------------
// SubAgentResult
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentResult {
    pub task_id: NodeId,
    pub success: bool,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: u64,
    pub tokens_used: usize,
    /// How many tools the sub-agent invoked during this run.
    #[serde(default)]
    pub tool_calls_made: usize,
    /// How many of those tool calls were write/modify operations.
    #[serde(default)]
    pub write_calls_made: usize,
    /// Graph-level errors the sub-agent discovered while reading L2 — e.g.
    /// "graph says A calls B but A's source doesn't call B". Empty for
    /// normal results. When non-empty, the dispatcher aggregates these
    /// into `DispatchOutcome.graph_errors` and the GraphLoop bubbles
    /// `LoopState::GraphInvalid { source: DuringExecution }` to the caller.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub graph_errors: Vec<GraphError>,
}

impl SubAgentResult {
    pub fn ok(task_id: NodeId, output: String, duration_ms: u64, tokens_used: usize) -> Self {
        Self {
            task_id,
            success: true,
            output,
            error: None,
            duration_ms,
            tokens_used,
            tool_calls_made: 0,
            write_calls_made: 0,
            graph_errors: Vec::new(),
        }
    }

    pub fn failure(task_id: NodeId, error: String, duration_ms: u64) -> Self {
        Self {
            task_id,
            success: false,
            output: String::new(),
            error: Some(error),
            duration_ms,
            tokens_used: 0,
            tool_calls_made: 0,
            write_calls_made: 0,
            graph_errors: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// SubAgent
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SubAgent {
    pub model: Arc<dyn Model>,
    pub temperature: f64,
    pub max_tokens: Option<usize>,
    /// Max graph distance to include in the local subgraph context.
    pub context_depth: usize,
    pub context_builder: ContextBuilder,
    /// Tool surface the sub-agent may invoke.
    pub tools: Arc<ToolRegistry>,
    pub policy: Arc<dyn Policy>,
    /// Optional prompt registry for dynamic prompt composition.
    pub prompt_registry: Option<Arc<crate::skills::prompt_registry::PromptRegistry>>,
    /// Optional write-scope guard. When set, every bash invocation is
    /// checked against the allowed paths before reaching the tool. Set
    /// per-agent via `with_scope`, or per-task via `with_task_scope`
    /// (the dispatcher uses the latter).
    pub scope_guard: Option<Arc<ScopeGuard>>,
    pub tool_cwd: PathBuf,
    pub tool_output_cap: usize,
    /// Cap on tool-loop iterations. Each iteration is one model call.
    /// When reached, the last model output is returned with `success=false`
    /// and an error noting the cap.
    pub max_steps: usize,
}

impl SubAgent {
    pub fn new(model: Arc<dyn Model>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            model,
            temperature: 0.3,
            max_tokens: Some(4096),
            context_depth: 2,
            context_builder: {
                let mut cb = ContextBuilder::new();
                cb.max_graph_depth = 2;
                cb
            },
            tools: Arc::new(ToolRegistry::new()),
            policy: Arc::new(DangerousCommandDeny::new()),
            scope_guard: None,
            prompt_registry: None,
            tool_cwd: cwd,
            tool_output_cap: 6_000,
            max_steps: 8,
        }
    }

    pub fn with_temperature(mut self, t: f64) -> Self {
        self.temperature = t;
        self
    }

    pub fn with_max_tokens(mut self, n: usize) -> Self {
        self.max_tokens = Some(n);
        self
    }

    /// Attach a [`ToolRegistry`]. The sub-agent will be told about every
    /// registered tool's schema in its system prompt; the `Policy` (set via
    /// [`with_policy`]) gates which calls actually execute.
    pub fn with_tools(mut self, tools: Arc<ToolRegistry>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_policy(mut self, policy: Arc<dyn Policy>) -> Self {
        self.policy = policy;
        self
    }

    /// Set a default scope guard on this agent. The guard will apply to
    /// every task the agent executes.
    pub fn with_scope(mut self, guard: Arc<ScopeGuard>) -> Self {
        self.scope_guard = Some(guard);
        self
    }

    /// Return a clone of this agent with the given scope guard
    /// installed. Used by the pool to give every task its own derived
    /// scope without mutating the shared `agent` Arc.
    pub fn with_task_scope(&self, guard: Arc<ScopeGuard>) -> SubAgent {
        let mut clone = self.clone();
        clone.scope_guard = Some(guard);
        clone
    }

    pub fn with_prompt_registry(mut self, pr: Arc<crate::skills::prompt_registry::PromptRegistry>) -> Self {
        self.prompt_registry = Some(pr);
        self
    }

    pub fn with_tool_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.tool_cwd = cwd.into();
        self
    }

    pub fn with_tool_output_cap(mut self, n: usize) -> Self {
        self.tool_output_cap = n;
        self
    }

    pub fn with_max_steps(mut self, n: usize) -> Self {
        self.max_steps = n.max(1);
        self
    }

    /// Execute one sub-task via the tool-calling loop.
    pub async fn execute(
        &self,
        task: &SubTask,
        world_graph: &Graph,
        loader: &dyn SourceLoader,
    ) -> Result<SubAgentResult> {
        let started = Instant::now();

        // Build the initial context bundle (L0 + L1 + L2 by distance).
        let overview = format!(
            "Parent graph snapshot: {} nodes / {} edges / {} L1 entries. \
             Your focus is on the local slice listed in the next sections.",
            world_graph.node_count(),
            world_graph.edge_count(),
            world_graph.l1.len(),
        );
        let context = self.context_builder.build(
            "", // role lives in the system message
            &overview,
            &task.description,
            "(Phase 4: prior batch results not yet plumbed in)",
            world_graph,
            &task.involved_nodes,
            loader,
        )?;

        let system_prompt = build_system_prompt(
            self.tools.as_ref(),
            self.max_steps,
            &task.role_prompt,
            self.prompt_registry.as_deref(),
        );
        let user_prompt = build_initial_user_prompt(
            task,
            &context.text,
            self.scope_guard.as_deref(),
        );

        let mut messages: Vec<Message> = vec![
            Message::system(system_prompt),
            Message::user(user_prompt),
        ];

        let mut tool_ctx = ToolContext::new(self.tool_cwd.clone())
            .with_policy(self.policy.clone())
            .with_max_output(self.tool_output_cap);
        tool_ctx.add_hook(Arc::new(crate::tools::LoggingHook::default()));
        tool_ctx.add_hook(Arc::new(crate::tools::StatsHook::default()));

        let mut tokens_used = 0usize;
        let mut tool_calls_made = 0usize;
        let mut write_calls_made = 0usize;

        for step in 0..self.max_steps {
            let req = ModelRequest {
                messages: messages.clone(),
                tools: Vec::new(), // we use JSON-action protocol, not OpenAI's tools field
                temperature: self.temperature,
                max_tokens: self.max_tokens,
                stop: Vec::new(),
            };

            let resp = match self.model.complete(req).await {
                Ok(r) => r,
                Err(e) => {
                    let duration_ms = started.elapsed().as_millis() as u64;
                    debug!(task_id = %task.id, step, error = %e, "sub-agent model call failed");
                    return Ok(SubAgentResult {
                        task_id: task.id.clone(),
                        success: false,
                        output: String::new(),
                        error: Some(format!("{e}")),
                        duration_ms,
                        tokens_used,
                        tool_calls_made,
                        write_calls_made,
                        graph_errors: Vec::new(),
                    });
                }
            };
            tokens_used += resp.usage.total_tokens;
            // Persist what the model said into the history.
            messages.push(Message::assistant(resp.content.clone()));

            match parse_action(&resp.content) {
                Action::FinalAnswer { answer, .. } => {
                    let mut outcome = task.contract.check(&answer);
                    if outcome.is_satisfied() {
                        outcome = task.contract.check_tool_calls(tool_calls_made, write_calls_made);
                    }
                    if !outcome.is_satisfied() {
                        // Contract failed. Feed the failure back as a user
                        // message so the model can retry. This counts as a
                        // step but does NOT increment tool_calls_made.
                        let reason = match outcome {
                            ContractOutcome::Failed(s) => s,
                            ContractOutcome::Satisfied => unreachable!(),
                        };
                        warn!(
                            task_id = %task.id,
                            step,
                            "sub-agent: contract check failed; feeding back for retry"
                        );
                        messages.push(Message::user(format!(
                            "Your final_answer did not satisfy the dispatch contract:\n\
                             {reason}\n\n\
                             Either revise the answer (emit another `final_answer`) or, if the \
                             contract is genuinely impossible, emit `report_graph_error` \
                             explaining why."
                        )));
                        continue;
                    }
                    let duration_ms = started.elapsed().as_millis() as u64;
                    info!(
                        task_id = %task.id,
                        step,
                        tool_calls_made,
                        write_calls_made,
                        tokens_used,
                        duration_ms,
                        "sub-agent emitted final_answer (contract satisfied)"
                    );
                    return Ok(SubAgentResult {
                        task_id: task.id.clone(),
                        success: true,
                        output: answer,
                        error: None,
                        duration_ms,
                        tokens_used,
                        tool_calls_made,
                        write_calls_made,
                        graph_errors: Vec::new(),
                    });
                }
                Action::ReportGraphError { errors, .. } => {
                    let duration_ms = started.elapsed().as_millis() as u64;
                    info!(
                        task_id = %task.id,
                        step,
                        error_count = errors.len(),
                        tokens_used,
                        duration_ms,
                        "sub-agent reported graph errors (bubbling up)"
                    );
                    // Tag every error with the discovering sub-task id.
                    let tagged: Vec<GraphError> = errors
                        .into_iter()
                        .map(|e| e.with_discovered_by(task.id.to_string()))
                        .collect();
                    return Ok(SubAgentResult {
                        task_id: task.id.clone(),
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "sub-agent reported {} graph error(s) instead of a final answer",
                            tagged.len()
                        )),
                        duration_ms,
                        tokens_used,
                        tool_calls_made,
                        write_calls_made,
                        graph_errors: tagged,
                    });
                }
                Action::UseTool { tool, args, .. } => {
                    tool_calls_made += 1;
                    // Track write calls for MustEdit contracts.
                    if let Some(t) = self.tools.get(&tool) {
                        if !t.is_read_only(&args) { write_calls_made += 1; }
                    }
                    // v2 spec §5.1: if this is a write task and
                    // the sub-agent has used 3+ tool calls
                    // without ever writing a file, inject a
                    // forceful reminder. This catches the
                    // failure mode observed in goal-driven
                    // testing where the sub-agent explores
                    // the codebase (`ls`, `find`) instead of
                    // writing the artifact.
                    if step >= 3
                        && write_calls_made == 0
                        && detect_task_kind(&task.description) == TaskKind::Write
                    {
                        warn!(
                            task_id = %task.id,
                            step,
                            write_calls_made,
                            "write task: sub-agent has not called write_file yet — injecting reminder"
                        );
                        messages.push(Message::user(
                            "⚠️ You are 3+ tool calls into a WRITE task and you have NOT \
                             called `write_file` or `edit_file` yet. Stop exploring NOW. \
                             Your next action MUST be `write_file` (or `edit_file`). \
                             If you have been reading files, you already have enough context — \
                             start writing. If you have not, you don't need more context; \
                             write the artifact now.".to_string(),
                        ));
                    }
                    debug!(task_id = %task.id, step, tool = %tool, "sub-agent calling tool");
                    // Scope check (only if a guard is attached).
                    if let Some(sg) = &self.scope_guard {
                        if let Err(v) = sg.check(&tool, &args) {
                            let detail = format!(
                                "Tool `{}` denied by scope guard: {}. \
                                 Stay within your allowed write paths.",
                                tool, v.reason
                            );
                            warn!(
                                task_id = %task.id,
                                step,
                                tool = %tool,
                                reason = %v.reason,
                                "scope guard denied tool call"
                            );
                            messages.push(Message::user(format!(
                                "{detail}\n\nContinue. Either call another tool, \
                                 emit final_answer, or report_graph_error if you \
                                 discovered a graph/code mismatch."
                            )));
                            continue;
                        }
                    }
                    let tool_msg = match self.tools.invoke(&tool, args, &tool_ctx).await {
                        Ok(out) => format!(
                            "Tool `{}` returned (exit_code={:?}, interrupted={}, duration_ms={}):\n{}",
                            tool, out.exit_code, out.interrupted, out.duration_ms, out.content
                        ),
                        Err(e) => format!("Tool `{}` errored: {}", tool, e),
                    };
                    messages.push(Message {
                        role: Role::User,
                        content: format!(
                            "{tool_msg}\n\nContinue. Either call another tool, emit final_answer, or report_graph_error if you discovered a graph/code mismatch."
                        ),
                        ..Default::default()
                    });
                }
                Action::ParseFailed => {
                    // The model didn't follow the JSON protocol — most
                    // likely it just wrote a result. Accept the raw content
                    // as the final answer (graceful degradation).
                    let duration_ms = started.elapsed().as_millis() as u64;
                    warn!(
                        task_id = %task.id,
                        step,
                        "sub-agent response not parseable as action JSON; treating as final answer"
                    );
                    return Ok(SubAgentResult {
                        task_id: task.id.clone(),
                        success: true,
                        output: resp.content,
                        error: None,
                        duration_ms,
                        tokens_used,
                        tool_calls_made,
                        write_calls_made,
                        graph_errors: Vec::new(),
                    });
                }
            }
        }

        // max_steps reached without final_answer. Pull the last assistant
        // utterance as a best-effort output, but mark as failure so the
        // dispatcher surfaces it.
        let duration_ms = started.elapsed().as_millis() as u64;
        warn!(
            task_id = %task.id,
            max_steps = self.max_steps,
            "sub-agent exhausted max_steps without final_answer"
        );
        let last = messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::Assistant))
            .map(|m| m.content.clone())
            .unwrap_or_default();
        Ok(SubAgentResult {
            task_id: task.id.clone(),
            success: false,
            output: last,
            error: Some(format!(
                "max_steps ({}) reached without final_answer",
                self.max_steps
            )),
            duration_ms,
            tokens_used,
            tool_calls_made,
                        write_calls_made,
            graph_errors: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

/// Try to load a prompt from a file, falling back to the hardcoded default.
/// This lets users edit `skills/prompts/subagent-*.md` without recompiling.
fn load_prompt_file(path: &str, default: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| default.to_string())
}

/// Fallback system prompt template for sub-agents. Used when
/// `skills/prompts/subagent-system.md` does not exist.
const SUBAGENT_SYSTEM_PROMPT_DEFAULT: &str = "You are a sub-agent in a graph-centric agent harness. You have been assigned ONE narrow sub-task with a local slice of the parent's relationship graph. Your job is to execute that sub-task and return a concise, useful result.\n\n{role_section}\n\n**CODE MODIFICATION TASKS**: You MUST actually edit files. Use dedicated tools:\n  - `read_file` to read any file (supports offset/limit for large files)\n  - `edit_file` to replace a string in a file (old_string must be unique in the file)\n  - `write_file` to create or overwrite a file with new content\n  - `bash` to run `cargo check --lib` to verify your changes compile\n  Do NOT use sed/cat via bash for file editing — use the dedicated tools instead.\n\nYou operate in a tool-calling loop. Each turn you emit exactly ONE structured JSON object as your entire response — no markdown fences, no prose around it. You can call a tool to gather information, emit your final answer, or — if you discover the graph itself is wrong — report a graph error instead.\n\n## File-reading strategy\n\n- `read_file` with a path to read any file. Use `offset` and `limit` for large files.\n- `bash` with `ls`, `find`, `grep -rn` for discovery and search.\n- Aim to read **3-5 files max** before emitting `final_answer`.\n- DO NOT repeat `ls` on the same directory more than once. If you've already seen the structure, the next bash call should be a `cat`/`head`/`grep` on a specific file, not another listing.\n- Aim to read **3-5 files max** before emitting `final_answer`. Don't browse aimlessly. The parent will use your summary to decide the next move.\n\n## Output schemas\n\n1) TOOL CALL — when you need to gather information:\n   {\"action\": \"use_tool\", \"tool\": \"<name>\", \"args\": {...}, \"thinking\": \"<one sentence why>\"}\n\n2) FINAL ANSWER — when you have enough information:\n   {\"action\": \"final_answer\", \"answer\": \"<your concise result>\", \"thinking\": \"<one sentence why complete>\"}\n\n3) REPORT GRAPH ERROR — when you discover the graph contradicts reality:\n   {\"action\": \"report_graph_error\",\n     \"errors\": [\n       {\n         \"kind\": \"L0Structural\" | \"L1Semantic\" | \"ScopeGap\",\n         \"l0_error_type\": \"MissingRelation\" | \"WrongRelation\" | \"MissingNode\",\n         \"detail\": \"<what's wrong>\",\n         \"related_nodes\": [\"<node_id>\"],\n         \"current_l1\": \"<what L1 said>\",\n         \"actual_l2_evidence\": \"<what L2 actually says>\"\n       }\n     ],\n     \"thinking\": \"<why this means the graph is wrong>\"}\n   Use this only when you have direct evidence (e.g., tool output showing the truth). Use it SPARINGLY — a single bubble-up triggers a parent-level Graph-phase repair, which is expensive.\n\n## Available tools\n{tools_block}\n\n## Discipline\n- Maximum {max_steps} tool calls per sub-task. After that you MUST emit final_answer (or report_graph_error if applicable).\n- Use tools sparingly — read what you need, then answer. Don't browse aimlessly.\n- You do NOT propose graph changes via patches. The parent owns the graph; you produce a result string OR report errors for the parent to fix.\n- **Match the user's language.** If the task description (or any user-facing text in this prompt) is in a non-English language, emit your `final_answer` in that same language. The parent's user will see your result directly; English content next to a Chinese task forces them to mentally translate.\n- If you cannot use any tool to make progress, emit final_answer with what you have.";

fn build_system_prompt(
    tools: &ToolRegistry,
    max_steps: usize,
    role_prompt: &str,
    registry: Option<&crate::skills::prompt_registry::PromptRegistry>,
) -> String {
    let defs = tools.defs();

    // Use registry with PromptContext for dynamic composition.
    // Role strings are namespaced ("subagent-edit" / "subagent-explore")
    // so they can't collide with the main proposer's role ("edit"),
    // which is also "edit" in proposer.rs. The main proposer doesn't
    // have file tools; if a "role-edit" prompt block were keyed to
    // the proposer's "edit" role, the main model would be told about
    // tools it doesn't have (see 9ca8470e failure).
    let role_section = if let Some(reg) = registry {
        let role = if role_prompt.contains("edit") || role_prompt.contains("code") || role_prompt.contains("修改") {
            "subagent-edit"
        } else if role_prompt.contains("explore") || role_prompt.contains("探索") || role_prompt.contains("调研") {
            "subagent-explore"
        } else {
            "auto"
        };
        let composed = reg.compose_role(role, role_prompt, false);
        if !composed.is_empty() { format!("\n{composed}\n") } else { String::new() }
    } else if !role_prompt.is_empty() {
        format!("\n## Role\n{role_prompt}\n")
    } else {
        String::new()
    };
    let tools_block = if defs.is_empty() {
        "(no tools registered — go straight to final_answer)".to_string()
    } else {
        let mut s = String::new();
        for d in &defs {
            s.push_str(&format!(
                "- `{}` — {}\n  args schema: {}\n",
                d.name,
                d.description,
                serde_json::to_string(&d.schema).unwrap_or_else(|_| "{}".into())
            ));
        }
        s
    };

    let template = load_prompt_file("skills/prompts/subagent-system.md", SUBAGENT_SYSTEM_PROMPT_DEFAULT);
    template
        .replace("{role_section}", &role_section)
        .replace("{tools_block}", &tools_block)
        .replace("{max_steps}", &max_steps.to_string())
}

fn build_initial_user_prompt(task: &SubTask, context_text: &str, scope: Option<&ScopeGuard>) -> String {
    let scope_section = match scope {
        Some(sg) if sg.is_active() => {
            let mut s = String::from("\n\n## Write scope\nEdits and writes are restricted to these paths:\n");
            for p in &sg.allowed_write_paths {
                s.push_str(&format!("- {}\n", p.display()));
            }
            s
        }
        _ => String::new(),
    };
    // v2 agent-harness spec §5.1: detect task kind from the
    // description and prepend a strong directive. The most
    // common failure mode observed in goal-driven testing was
    // sub-agents using `bash ls` for write/create tasks — they
    // explored the codebase instead of producing artifacts.
    // Detecting the kind and prepending a "first action =
    // write_file" directive fixes this for the most common
    // task shapes without limiting the agent's flexibility
    // for genuinely exploratory tasks.
    let task_kind = detect_task_kind(&task.description);
    let directive = match task_kind {
        TaskKind::Write => "\n\n🚨 **THIS IS A WRITE/CREATE TASK. DO NOT EXPLORE FIRST.**\n\
                            Your first action MUST be `write_file` (or `edit_file` if a file already exists). \
                            Do NOT `ls`, do NOT `find`, do NOT browse. The user wants the artifact, not exploration.\n\
                            Pattern: 1) `write_file(path=..., content=...)` → 2) `bash` to verify (e.g. `go build` or `cargo check`) → 3) `final_answer`.\n",
        TaskKind::Modify => "\n\n🔧 **THIS IS A MODIFY TASK.** Read the relevant file(s) first (max 1-2 files), \
                            then `edit_file` to apply the change, then `bash` to verify, then `final_answer`. \
                            Do NOT browse the whole repo — focus on the files you actually need to change.\n",
        TaskKind::Read => "\n\n🔍 **THIS IS A READ/ANALYZE TASK.** Use `read_file` (with `offset`/`limit` for large files) or `bash` \
                           with `grep`/`head`/`cat`. After 2-3 reads, emit `final_answer`.\n",
        TaskKind::Unknown => "",
    };
    format!(
        "{directive}{context}\n\n## Your sub-task ({task_id})\n{desc}{scope_section}\n\n\
         Begin. Emit your first JSON action now.",
        directive = directive,
        context = context_text,
        task_id = task.id,
        desc = task.description,
        scope_section = scope_section,
    )
}

/// Heuristic task-kind classifier. Used by `build_initial_user_prompt`
/// to choose the strongest possible opening directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskKind {
    /// Create a new file / write a new artifact.
    Write,
    /// Modify an existing file.
    Modify,
    /// Read or analyze code/data.
    Read,
    /// Couldn't classify.
    Unknown,
}

fn detect_task_kind(description: &str) -> TaskKind {
    let d = description.to_lowercase();
    // Chinese + English keywords for "write/create/implement"
    let write_kw = [
        "实现", "写", "创建", "新建", "生成", "产出",
        "implement", "create", "write", "build", "add", "generate", "produce", "scaffold",
    ];
    // "modify/edit/fix/refactor/change"
    let modify_kw = [
        "修改", "改", "修复", "重构", "调整", "优化",
        "modify", "edit", "fix", "refactor", "change", "update", "adjust", "optimize",
    ];
    // "read/analyze/search/inspect/look at"
    let read_kw = [
        "分析", "读", "查看", "搜索", "研究", "探索", "检查",
        "analyze", "read", "search", "investigate", "inspect", "look", "find",
    ];
    if write_kw.iter().any(|k| d.contains(k)) {
        TaskKind::Write
    } else if modify_kw.iter().any(|k| d.contains(k)) {
        TaskKind::Modify
    } else if read_kw.iter().any(|k| d.contains(k)) {
        TaskKind::Read
    } else {
        TaskKind::Unknown
    }
}

#[cfg(test)]
mod task_kind_tests {
    use super::*;
    #[test]
    fn detects_chinese_write() {
        assert_eq!(detect_task_kind("实现: Go 单文件 todo 工具"), TaskKind::Write);
        assert_eq!(detect_task_kind("写一个 Python 脚本"), TaskKind::Write);
    }
    #[test]
    fn detects_english_modify() {
        assert_eq!(detect_task_kind("refactor the auth module"), TaskKind::Modify);
        assert_eq!(detect_task_kind("fix the bug in main.rs"), TaskKind::Modify);
    }
    #[test]
    fn detects_read() {
        assert_eq!(detect_task_kind("分析 GraphLoop 行为"), TaskKind::Read);
        assert_eq!(detect_task_kind("analyze the codebase"), TaskKind::Read);
    }
    #[test]
    fn unknown_when_no_keyword() {
        assert_eq!(detect_task_kind("do the thing"), TaskKind::Unknown);
    }
}

// ---------------------------------------------------------------------------
// JSON action parsing
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Action {
    UseTool {
        tool: String,
        args: serde_json::Value,
        #[allow(dead_code)]
        thinking: String,
    },
    FinalAnswer {
        answer: String,
        #[allow(dead_code)]
        thinking: String,
    },
    ReportGraphError {
        errors: Vec<GraphError>,
        #[allow(dead_code)]
        thinking: String,
    },
    ParseFailed,
}

fn parse_action(text: &str) -> Action {
    let cleaned = match extract_json_block(text) {
        Ok(s) => s,
        Err(_) => return Action::ParseFailed,
    };
    let value: serde_json::Value = match serde_json::from_str(&cleaned) {
        Ok(v) => v,
        Err(_) => return Action::ParseFailed,
    };
    let action_str = match value.get("action").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return Action::ParseFailed,
    };
    let thinking = value
        .get("thinking")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match action_str {
        "use_tool" => {
            let tool = value
                .get("tool")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if tool.is_empty() {
                return Action::ParseFailed;
            }
            let args = value
                .get("args")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            Action::UseTool {
                tool,
                args,
                thinking,
            }
        }
        "final_answer" => {
            let answer = value
                .get("answer")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Action::FinalAnswer { answer, thinking }
        }
        "report_graph_error" => {
            let errors_v = match value.get("errors").and_then(|v| v.as_array()) {
                Some(arr) => arr,
                None => return Action::ParseFailed,
            };
            let mut errors = Vec::with_capacity(errors_v.len());
            for e in errors_v {
                if let Some(ge) = parse_graph_error_from_subagent(e) {
                    errors.push(ge);
                }
            }
            if errors.is_empty() {
                return Action::ParseFailed;
            }
            Action::ReportGraphError { errors, thinking }
        }
        _ => Action::ParseFailed,
    }
}

/// Build a `GraphError` from the JSON shape sub-agents are told to emit.
/// Unknown kinds default to `L0Structural` with `MissingRelation`.
fn parse_graph_error_from_subagent(v: &serde_json::Value) -> Option<GraphError> {
    let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("L0Structural");
    let detail = v
        .get("detail")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if detail.is_empty() {
        return None;
    }
    let related: Vec<NodeId> = v
        .get("related_nodes")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(NodeId::from))
                .collect()
        })
        .unwrap_or_default();
    match kind {
        "L1Semantic" => {
            let node = related.first().cloned()?;
            let current_l1 = v
                .get("current_l1")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let actual_l2_evidence = v
                .get("actual_l2_evidence")
                .and_then(|x| x.as_str())
                .unwrap_or(detail.as_str())
                .to_string();
            Some(GraphError::L1Semantic {
                node,
                current_l1,
                actual_l2_evidence,
                discovered_by: None,
            })
        }
        "ScopeGap" => Some(GraphError::ScopeGap {
            missing_nodes: related,
            missing_edges: Vec::new(),
            detail,
            discovered_by: None,
        }),
        _ /* L0Structural */ => {
            let l0t = v
                .get("l0_error_type")
                .and_then(|x| x.as_str())
                .unwrap_or("MissingRelation");
            let error_type = match l0t {
                "WrongRelation" => L0ErrorType::WrongRelation,
                "MissingNode" => L0ErrorType::MissingNode,
                _ => L0ErrorType::MissingRelation,
            };
            Some(GraphError::L0Structural {
                error_type,
                detail,
                related_nodes: related,
                discovered_by: None,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::InMemorySources;
    use crate::graph::{Edge, RelationType};
    use crate::model::{FinishReason, ModelResponse, Usage};
    use crate::tools::{BashTool, PolicyDecision};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn _assert_subagent_is_send_sync() {
        fn assert_ss<T: Send + Sync>() {}
        assert_ss::<SubAgent>();
    }

    struct MockModel {
        responses: Mutex<Vec<String>>,
        fail_next: Mutex<bool>,
    }

    impl MockModel {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().rev().map(String::from).collect()),
                fail_next: Mutex::new(false),
            }
        }
        fn failing() -> Self {
            Self {
                responses: Mutex::new(Vec::new()),
                fail_next: Mutex::new(true),
            }
        }
    }

    #[async_trait]
    impl Model for MockModel {
        fn name(&self) -> &str {
            "mock-subagent"
        }
        async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse> {
            if *self.fail_next.lock().unwrap() {
                return Err(HarnessError::model("simulated network failure"));
            }
            let content = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| r#"{"action":"final_answer","answer":"default","thinking":""}"#.to_string());
            Ok(ModelResponse {
                content,
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                reasoning_content: None,
                usage: Usage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    ..Default::default()
                },
            })
        }
    }

    fn empty_loader() -> Arc<dyn SourceLoader> {
        Arc::new(InMemorySources(HashMap::new()))
    }

    fn world_with_three_nodes() -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::file("a", "A"));
        g.add_node(Node::file("b", "B"));
        g.add_node(Node::file("c", "C"));
        g.add_edge(Edge::new("a", "b", RelationType::Imports, 0.9, "")).unwrap();
        g.add_edge(Edge::new("b", "c", RelationType::Calls, 0.8, "")).unwrap();
        g
    }

    fn sample_subtask() -> SubTask {
        SubTask {
            id: NodeId::from("t1"),
            description: "Analyze module A and report on its role".into(),
            involved_nodes: vec![NodeId::from("a")],
            needs: TaskNeeds::read_only(),
            contract: CheckContract::default(),
            role_prompt: String::new(),
        }
    }

    #[test]
    fn subtask_round_trips_through_task_node() {
        let st = sample_subtask();
        let node = st.to_task_node();
        let st2 = SubTask::from_task_node(&node).unwrap();
        assert_eq!(st2.id, st.id);
        assert_eq!(st2.description, st.description);
        assert_eq!(st2.involved_nodes, st.involved_nodes);
        assert_eq!(st2.needs.can_read, st.needs.can_read);
    }

    #[test]
    fn subtask_round_trips_with_contract() {
        // A SubTask with a KnowHow contract must survive
        // `to_task_node` → `from_task_node` round-trip with the
        // contract intact. This is how the dispatcher sees contracts
        // for tasks it didn't construct itself.
        let st = SubTask {
            id: NodeId::from("t1"),
            description: "analyze module A".into(),
            involved_nodes: vec![NodeId::from("a")],
            needs: TaskNeeds::read_only(),
            contract: CheckContract::KnowHow {
                must_mention_any: vec!["module A".into()],
                min_length: 20,
            },
            role_prompt: String::new(),
        };
        let node = st.to_task_node();
        let st2 = SubTask::from_task_node(&node).unwrap();
        match &st2.contract {
            CheckContract::KnowHow { must_mention_any, min_length } => {
                assert_eq!(must_mention_any, &vec!["module A".to_string()]);
                assert_eq!(*min_length, 20);
            }
            other => panic!("expected KnowHow, got {other:?}"),
        }
    }

    #[test]
    fn subtask_contract_defaults_to_none() {
        // A freshly-constructed SubTask with no contract field set
        // must default to CheckContract::None. The task graph nodes
        // the decomposer emits today don't have a `contract` metadata
        // key, so from_task_node has to handle the absence gracefully.
        let st = SubTask {
            id: NodeId::from("t1"),
            description: "x".into(),
            involved_nodes: vec![],
            needs: TaskNeeds::default(),
            contract: CheckContract::default(),
            role_prompt: String::new(),
        };
        let node = st.to_task_node();
        let st2 = SubTask::from_task_node(&node).unwrap();
        assert!(matches!(st2.contract, CheckContract::None));
    }

    #[test]
    fn subtask_from_node_handles_missing_metadata_gracefully() {
        let plain = Node::task("t1", "plain task");
        let st = SubTask::from_task_node(&plain).unwrap();
        assert_eq!(st.id, NodeId::from("t1"));
        assert!(st.involved_nodes.is_empty());
        assert!(!st.needs.can_write);
    }

    #[tokio::test]
    async fn final_answer_on_first_turn_returns_success() {
        let resp = r#"{"action":"final_answer","answer":"analysis: module A handles X","thinking":"clear from context"}"#;
        let model: Arc<dyn Model> = Arc::new(MockModel::new(vec![resp]));
        let agent = SubAgent::new(model);
        let result = agent
            .execute(&sample_subtask(), &world_with_three_nodes(), empty_loader().as_ref())
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output, "analysis: module A handles X");
        assert_eq!(result.tool_calls_made, 0);
        assert_eq!(result.tokens_used, 150);
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn plain_text_response_treated_as_final_answer() {
        // Model ignored the JSON protocol; we accept the raw text as the answer.
        let model: Arc<dyn Model> = Arc::new(MockModel::new(vec![
            "Module A handles auth — it imports B for utilities.",
        ]));
        let agent = SubAgent::new(model);
        let result = agent
            .execute(&sample_subtask(), &world_with_three_nodes(), empty_loader().as_ref())
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Module A"));
        assert_eq!(result.tool_calls_made, 0);
    }

    #[tokio::test]
    async fn tool_call_then_final_answer_runs_two_steps() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(BashTool::new()));
        let tools = Arc::new(reg);

        let call_bash = r#"{"action":"use_tool","tool":"bash","args":{"command":"echo from_tool"},"thinking":"need to see"}"#;
        let finalize =
            r#"{"action":"final_answer","answer":"got from_tool from bash","thinking":"have what I need"}"#;
        let model: Arc<dyn Model> = Arc::new(MockModel::new(vec![call_bash, finalize]));
        let agent = SubAgent::new(model).with_tools(tools);

        let result = agent
            .execute(&sample_subtask(), &world_with_three_nodes(), empty_loader().as_ref())
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.tool_calls_made, 1);
        assert!(result.output.contains("from_tool"));
        // Two model calls × 150 tokens each
        assert_eq!(result.tokens_used, 300);
    }

    #[tokio::test]
    async fn unknown_tool_invocation_returned_to_model_as_error() {
        // First action calls a tool that doesn't exist; SubAgent feeds back the
        // error and the second action emits final_answer.
        let call_ghost = r#"{"action":"use_tool","tool":"nonexistent","args":{},"thinking":"trying"}"#;
        let finalize =
            r#"{"action":"final_answer","answer":"tool not available; default answer","thinking":"recovered"}"#;
        let model: Arc<dyn Model> = Arc::new(MockModel::new(vec![call_ghost, finalize]));
        let agent = SubAgent::new(model);
        let result = agent
            .execute(&sample_subtask(), &world_with_three_nodes(), empty_loader().as_ref())
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("not available"));
        assert_eq!(result.tool_calls_made, 1);
    }

    #[tokio::test]
    async fn policy_denied_tool_call_returns_error_to_model() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(BashTool::new()));
        let tools = Arc::new(reg);

        // ReadOnly policy will deny `rm` (not in read-only list).
        let call_rm = r#"{"action":"use_tool","tool":"bash","args":{"command":"rm -rf /tmp/xxx-nonexistent"},"thinking":"cleanup"}"#;
        let finalize =
            r#"{"action":"final_answer","answer":"could not delete; reporting anyway","thinking":"policy blocked"}"#;
        let model: Arc<dyn Model> = Arc::new(MockModel::new(vec![call_rm, finalize]));
        let agent = SubAgent::new(model).with_tools(tools).with_policy(Arc::new(crate::tools::ReadOnly));
        let result = agent
            .execute(&sample_subtask(), &world_with_three_nodes(), empty_loader().as_ref())
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.tool_calls_made, 1);
    }

    #[tokio::test]
    async fn max_steps_exhaustion_returns_failure_with_last_output() {
        // Model keeps calling tools, never emits final_answer. After max_steps
        // we get a failure with the last assistant message and an error string.
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(BashTool::new()));
        let tools = Arc::new(reg);

        let call_pwd = r#"{"action":"use_tool","tool":"bash","args":{"command":"pwd"},"thinking":"x"}"#;
        let model: Arc<dyn Model> = Arc::new(MockModel::new(vec![
            call_pwd, call_pwd, call_pwd,
        ]));
        let agent = SubAgent::new(model).with_tools(tools).with_max_steps(3);
        let result = agent
            .execute(&sample_subtask(), &world_with_three_nodes(), empty_loader().as_ref())
            .await
            .unwrap();
        assert!(!result.success);
        assert_eq!(result.tool_calls_made, 3);
        let err = result.error.expect("error string set");
        assert!(err.contains("max_steps"));
    }

    #[tokio::test]
    async fn model_error_captured_as_non_success_result() {
        let model: Arc<dyn Model> = Arc::new(MockModel::failing());
        let agent = SubAgent::new(model);
        let result = agent
            .execute(&sample_subtask(), &world_with_three_nodes(), empty_loader().as_ref())
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("simulated network failure"));
        assert!(result.output.is_empty());
    }

    #[tokio::test]
    async fn empty_tool_registry_means_no_loop_just_final_answer() {
        let resp = r#"{"action":"final_answer","answer":"done","thinking":"no tools needed"}"#;
        let model: Arc<dyn Model> = Arc::new(MockModel::new(vec![resp]));
        let agent = SubAgent::new(model); // default = empty registry
        let result = agent
            .execute(&sample_subtask(), &world_with_three_nodes(), empty_loader().as_ref())
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.tool_calls_made, 0);
    }

    #[test]
    fn parse_action_use_tool() {
        let s = r#"{"action":"use_tool","tool":"bash","args":{"command":"ls"},"thinking":"x"}"#;
        match parse_action(s) {
            Action::UseTool { tool, args, .. } => {
                assert_eq!(tool, "bash");
                assert_eq!(args.get("command").and_then(|v| v.as_str()), Some("ls"));
            }
            other => panic!("expected UseTool, got {other:?}"),
        }
    }

    #[test]
    fn parse_action_final_answer() {
        let s = r#"{"action":"final_answer","answer":"hello","thinking":"done"}"#;
        match parse_action(s) {
            Action::FinalAnswer { answer, .. } => assert_eq!(answer, "hello"),
            other => panic!("expected FinalAnswer, got {other:?}"),
        }
    }

    #[test]
    fn parse_action_use_tool_missing_tool_falls_to_parse_failed() {
        let s = r#"{"action":"use_tool","args":{}}"#;
        assert!(matches!(parse_action(s), Action::ParseFailed));
    }

    #[test]
    fn parse_action_unknown_action_falls_to_parse_failed() {
        let s = r#"{"action":"meditate","mantra":"om"}"#;
        assert!(matches!(parse_action(s), Action::ParseFailed));
    }

    #[test]
    fn parse_action_plain_text_falls_to_parse_failed() {
        assert!(matches!(parse_action("just some words"), Action::ParseFailed));
    }

    #[test]
    fn parse_action_handles_markdown_fence() {
        let s = "```json\n{\"action\":\"final_answer\",\"answer\":\"x\"}\n```";
        assert!(matches!(parse_action(s), Action::FinalAnswer { .. }));
    }

    #[test]
    fn parse_action_report_graph_error_with_l0() {
        let s = r#"{
            "action":"report_graph_error",
            "errors":[
                {"kind":"L0Structural","l0_error_type":"WrongRelation","detail":"A doesn't actually call B","related_nodes":["a","b"]}
            ],
            "thinking":"L2 contradicts L0"
        }"#;
        match parse_action(s) {
            Action::ReportGraphError { errors, .. } => {
                assert_eq!(errors.len(), 1);
                match &errors[0] {
                    GraphError::L0Structural { error_type, related_nodes, detail, .. } => {
                        assert!(matches!(error_type, L0ErrorType::WrongRelation));
                        assert_eq!(related_nodes.len(), 2);
                        assert!(detail.contains("doesn't actually call"));
                    }
                    other => panic!("expected L0Structural, got {other:?}"),
                }
            }
            other => panic!("expected ReportGraphError, got {other:?}"),
        }
    }

    #[test]
    fn parse_action_report_graph_error_with_l1_semantic() {
        let s = r#"{
            "action":"report_graph_error",
            "errors":[
                {"kind":"L1Semantic","detail":"drift","related_nodes":["a"],"current_l1":"says X","actual_l2_evidence":"says Y"}
            ]
        }"#;
        match parse_action(s) {
            Action::ReportGraphError { errors, .. } => {
                match &errors[0] {
                    GraphError::L1Semantic { node, current_l1, actual_l2_evidence, .. } => {
                        assert_eq!(node, &NodeId::from("a"));
                        assert_eq!(current_l1, "says X");
                        assert_eq!(actual_l2_evidence, "says Y");
                    }
                    other => panic!("expected L1Semantic, got {other:?}"),
                }
            }
            other => panic!("expected ReportGraphError, got {other:?}"),
        }
    }

    #[test]
    fn parse_action_report_graph_error_with_empty_errors_falls_to_parse_failed() {
        let s = r#"{"action":"report_graph_error","errors":[]}"#;
        assert!(matches!(parse_action(s), Action::ParseFailed));
    }

    #[tokio::test]
    async fn subagent_emitting_report_graph_error_returns_non_success_with_errors() {
        let resp = r#"{
            "action":"report_graph_error",
            "errors":[{"kind":"L0Structural","l0_error_type":"MissingRelation","detail":"missing call edge","related_nodes":["a"]}],
            "thinking":"discovered while reading source"
        }"#;
        let model: Arc<dyn Model> = Arc::new(MockModel::new(vec![resp]));
        let agent = SubAgent::new(model);
        let result = agent
            .execute(&sample_subtask(), &world_with_three_nodes(), empty_loader().as_ref())
            .await
            .unwrap();
        assert!(!result.success);
        assert_eq!(result.graph_errors.len(), 1);
        // discovered_by is set to the sub-task id by execute()
        assert_eq!(result.graph_errors[0].discovered_by(), Some("t1"));
        // Tool calls should be zero (errored on first turn)
        assert_eq!(result.tool_calls_made, 0);
        // Error string explains what happened
        assert!(result.error.unwrap().contains("graph error"));
    }

    #[test]
    fn subagent_result_serializes_compactly() {
        let r = SubAgentResult::ok(NodeId::from("t1"), "ok".into(), 42, 100);
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("\"error\""));
        assert!(s.contains("\"success\":true"));
        assert!(s.contains("\"tool_calls_made\":0"));
    }

    #[test]
    fn subagent_new_defaults_to_dangerous_command_deny_policy() {
        // A fresh SubAgent must not default to AllowAll — the system
        // would silently pass `rm -rf /` through. The new default is
        // DangerousCommandDeny. Verified by behavior (deny decision on
        // a known dangerous command) since `policy` is `Arc<dyn Policy>`
        // and the `pattern_names` helper is a method on the concrete
        // `DangerousCommandDeny` struct.
        let model: Arc<dyn Model> = Arc::new(MockModel::failing());
        let agent = SubAgent::new(model);
        let decision = agent.policy.decide(
            "bash",
            &serde_json::json!({"command": "rm -rf /"}),
            false,
        );
        match decision {
            PolicyDecision::Deny(reason) => {
                assert!(
                    reason.contains("rm-rf-root"),
                    "default deny should block `rm -rf /` via the rm-rf-root pattern, got: {reason}"
                );
            }
            other => panic!("expected Deny for `rm -rf /`, got {other:?}"),
        }
    }

    #[test]
    fn with_task_scope_clones_and_attaches_guard() {
        let model: Arc<dyn Model> = Arc::new(MockModel::failing());
        let agent = SubAgent::new(model);
        assert!(agent.scope_guard.is_none());
        let guard = Arc::new(ScopeGuard::new(vec![PathBuf::from("/proj/src")]));
        let scoped = agent.with_task_scope(guard.clone());
        assert!(scoped.scope_guard.is_some());
        // Original is untouched.
        assert!(agent.scope_guard.is_none());
    }

    #[tokio::test]
    async fn scope_violation_feeds_message_back_to_model() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(BashTool::new()));
        let tools = Arc::new(reg);
        let guard = Arc::new(ScopeGuard::new(vec![PathBuf::from("/proj/src")]));

        let call_out_of_scope = r#"{"action":"use_tool","tool":"bash","args":{"command":"rm /etc/passwd"},"thinking":"oops"}"#;
        let recover = r#"{"action":"final_answer","answer":"scope blocked me; reporting","thinking":"saw the scope denial"}"#;
        let model: Arc<dyn Model> = Arc::new(MockModel::new(vec![call_out_of_scope, recover]));

        let agent = SubAgent::new(model)
            .with_tools(tools)
            .with_task_scope(guard);
        let result = agent
            .execute(&sample_subtask(), &world_with_three_nodes(), empty_loader().as_ref())
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("scope blocked"));
        // The bash call was attempted but the scope guard denied it;
        // tool_calls_made counts the attempt, not the actual execution.
        assert_eq!(result.tool_calls_made, 1);
    }

    #[test]
    fn prompt_does_not_mention_task_needs_capabilities() {
        // Build a minimal context string and a sample task.
        let task = sample_subtask();
        let prompt = build_initial_user_prompt(&task, "context here", None);
        assert!(!prompt.contains("Capabilities you've been granted"),
            "prompt should not advertise TaskNeeds capabilities");
        assert!(!prompt.contains("can_read"),
            "prompt should not expose can_read bool");
        assert!(!prompt.contains("can_write"),
            "prompt should not expose can_write bool");
    }

    #[test]
    fn prompt_includes_scope_summary_when_guard_set() {
        let task = sample_subtask();
        let guard = ScopeGuard::new(vec![PathBuf::from("/proj/src")]);
        let prompt = build_initial_user_prompt(&task, "ctx", Some(&guard));
        assert!(prompt.contains("## Write scope"));
        assert!(prompt.contains("/proj/src"));
    }

    #[test]
    fn prompt_omits_scope_section_when_guard_inactive() {
        let task = sample_subtask();
        let guard = ScopeGuard::new(Vec::new()); // inactive
        let prompt = build_initial_user_prompt(&task, "ctx", Some(&guard));
        assert!(!prompt.contains("## Write scope"));
    }

    #[tokio::test]
    async fn contract_failure_feeds_message_back_to_model_for_retry() {
        // SubTask carries a KnowHow contract that requires mentioning
        // "auth.rs". The sub-agent's first final_answer doesn't mention
        // it; the sub-agent should feed the failure back and let the
        // model retry. The second final_answer mentions it; success.
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(BashTool::new()));
        let tools = Arc::new(reg);

        let first_wrong = r#"{"action":"final_answer","answer":"looked around, nothing relevant","thinking":"x"}"#;
        let second_right = r#"{"action":"final_answer","answer":"found auth.rs handles the auth path","thinking":"got it"}"#;
        let model: Arc<dyn Model> = Arc::new(MockModel::new(vec![first_wrong, second_right]));
        let agent = SubAgent::new(model).with_tools(tools).with_max_steps(5);

        let mut st = sample_subtask();
        st.contract = CheckContract::KnowHow {
            must_mention_any: vec!["auth.rs".into()],
            min_length: 5,
        };
        let result = agent
            .execute(&st, &world_with_three_nodes(), empty_loader().as_ref())
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("auth.rs"));
        // 2 model calls: one for the failed first attempt, one for the retry.
        assert_eq!(result.tool_calls_made, 0);
        assert!(result.tokens_used >= 300);
    }

    #[tokio::test]
    async fn contract_failure_at_max_steps_marks_result_as_failure() {
        // Sub-agent keeps emitting final_answers that fail the contract;
        // eventually max_steps is reached and the result is
        // `success: false` with a contract-violation error string.
        let first_wrong = r#"{"action":"final_answer","answer":"no idea","thinking":""}"#;
        let second_wrong = r#"{"action":"final_answer","answer":"still nothing","thinking":""}"#;
        let model: Arc<dyn Model> =
            Arc::new(MockModel::new(vec![first_wrong, second_wrong]));
        let agent = SubAgent::new(model).with_max_steps(2);

        let mut st = sample_subtask();
        st.contract = CheckContract::KnowHow {
            must_mention_any: vec!["auth.rs".into()],
            min_length: 5,
        };
        let result = agent
            .execute(&st, &world_with_three_nodes(), empty_loader().as_ref())
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.expect("error string set");
        assert!(
            err.contains("contract") || err.contains("max_steps"),
            "expected contract or max_steps error, got: {err}"
        );
    }
}
