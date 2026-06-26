//! GraphProposer — the model-driven step engine inside the iterative loop.
//!
//! Each `next_step` call:
//!
//! 1. Builds a `ModelRequest` from (system prompt + current graph snapshot +
//!    conversation history).
//! 2. Calls the model.
//! 3. Parses the response as exactly one [`ProposerStep`] — JSON object with
//!    a discriminated `step` field.
//!
//! The four possible steps map directly to the four moves any layer of the
//! iterative loop can make (see [[feedback-iterative-loop-is-centerpiece]]):
//!
//! - `AskUser`         — surface a question to the user (main agent only)
//! - `CallTool`        — invoke a tool from the registry to gather evidence
//! - `ProposePatch`    — append a small, surgical [`GraphPatch`] to the graph
//! - `ReadyForVerify`  — declare the build/extend phase done; hand off to
//!                       the Verifier
//!
//! The `GraphLoop` (next milestone) owns the side-effects: it actually
//! prompts the user, invokes the tool, applies the patch, or transitions
//! to verification. The proposer itself is pure: input model state → step.

use super::Conversation;
use crate::error::{HarnessError, Result};
use crate::graph::{DrillDownMark, Edge, Graph, GraphPatch, Node, NodeId, NodeKind, RelationType};
use crate::model::{Message, Model, Role};
use crate::tools::ToolRegistry;
use tracing::warn;
use std::sync::Arc;
use tracing::debug;

// ---------------------------------------------------------------------------
// Step type
// ---------------------------------------------------------------------------

/// One item in an `Explore` step's parallel-dispatch list. Each
/// item gets its own subagent (read-only, capped at 6 tool
/// calls), and the subagents run concurrently.
#[derive(Debug, Clone)]
pub struct ExploreItem {
    /// What to look at. Free-form: a directory path, a file
    /// glob, a function name, a list of node ids — whatever
    /// scopes the question. The subagent interprets.
    pub scope: String,
    /// The specific question the subagent should answer. A
    /// good pair is one a subagent can resolve in 3-6 tool
    /// calls.
    pub question: String,
}

/// Hard cap on the number of items in a single `Explore` step.
/// At 1 (per user override 2026-06-08), each Explore step dispatches
/// exactly one subagent — forcing the main agent to do hierarchical
/// decomposition (scan → list → read) instead of front-loading
/// everything into one subagent. Without this, the model packs
/// "list the whole repo + read every key file" into a single item,
/// the subagent runs 17+ steps and dumps 800k+ tokens back into
/// the main conversation, eventually blowing past the LLM's
/// instruction-following range and triggering "no '{' in response"
/// death.
const MAX_EXPLORE_ITEMS_PER_STEP: usize = 1;

/// Hard cap on per-item question length. Generous enough for
/// structured questions with sub-bullets (which the model
/// naturally writes at 1500-2000 chars for "compare two systems"
/// prompts); still small enough to keep the whole step's JSON
/// well-formed. Exceeding this triggers a `parse_step` error
/// that names the cap and tells the model to split into
/// multiple focused items.
const MAX_EXPLORE_QUESTION_CHARS: usize = 2000;

#[derive(Debug, Clone)]
pub enum ProposerStep {
    AskUser {
        question: String,
        /// Optional structured choices the user can pick from.
        /// When present, the frontend renders these as clickable buttons.
        options: Vec<String>,
        rationale: String,
    },
    CallTool {
        tool: String,
        args: serde_json::Value,
        rationale: String,
    },
    ProposePatch {
        patch: GraphPatch,
        rationale: String,
    },
    ReadyForVerify {
        rationale: String,
    },
    /// Self-pause with a specific blocker. The model is saying
    /// "I have enough context to know what's going on, but I cannot
    /// proceed without a specific human input" — a credential, a
    /// UX choice, paywalled source output, etc. Different from
    /// `AskUser` (which is for clarifying the task) and from
    /// `ReadyForVerify` (which is "I think the graph is complete"):
    /// this is "I am explicitly blocked on something the user must
    /// provide." The run pauses with the reason visible to the user.
    Block {
        /// Short label of what the model is blocked on (e.g.
        /// "missing API key", "need UX decision", "paywalled
        /// source").
        reason: String,
        /// Optional one-line question to surface to the user. If
        /// empty, the user just sees the reason and decides whether
        /// to unblock manually.
        needed_from_user: String,
        rationale: String,
    },
    /// Dispatch one or more subagents to do multi-file exploration
    /// on the model's behalf (Claude Code's `EXPLORE_AGENT`
    /// pattern, with parallel subagent fan-out). Use this when
    /// the model has seen a directory listing and needs to read
    /// several files to answer an open-ended question.
    ///
    /// When the model emits multiple items with disjoint scopes
    /// (e.g. "read src/agent AND read src/web"), the items are
    /// dispatched **in parallel** as separate subagents — the
    /// main agent's context stays clean (just one summary user
    /// message with all the results), but the wall-clock
    /// latency is roughly the slowest single subagent, not the
    /// sum. This is a key difference from a single big
    /// subagent that has to choose between scopes.
    Explore {
        /// One or more (scope, question) pairs. Each runs in
        /// its own subagent. Aim for items that are
        /// independent — if B depends on A's output, do them
        /// in two separate `Explore` steps so the second
        /// subagent sees the first's summary in the main
        /// conversation.
        items: Vec<ExploreItem>,
        rationale: String,
    },
    /// Consult the independent **advisor** model with a question. Use
    /// when the main (task) model wants a second opinion on a design or
    /// knowledge question. The advisor only answers — it never modifies
    /// the graph. Its answer is injected into the conversation and the
    /// main model decides what to do next. No-op (with a hint) when no
    /// advisor backend is configured.
    ConsultAdvisor {
        /// The question to ask the advisor.
        question: String,
        /// Optional extra context to give the advisor (relevant graph
        /// state, what was tried, constraints). May be empty.
        context: String,
        rationale: String,
    },
}

impl ProposerStep {
    /// Short label for logs and transcripts.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AskUser { .. } => "ask_user",
            Self::CallTool { .. } => "call_tool",
            Self::ProposePatch { .. } => "propose_patch",
            Self::ReadyForVerify { .. } => "ready_for_verify",
            Self::Block { .. } => "block",
            Self::Explore { .. } => "explore",
            Self::ConsultAdvisor { .. } => "consult_advisor",
        }
    }
}

// ---------------------------------------------------------------------------
// Proposer
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct GraphProposer {
    pub model: Arc<dyn Model>,
    pub tools: Arc<ToolRegistry>,
    /// Optional storage of past successful-run skills. When set, the
    /// system prompt includes a "## Available skills" section listing
    /// them. When `None`, no section is included.
    pub skills: Option<std::sync::Arc<dyn crate::skills::SkillStorage>>,
    /// Optional PromptRegistry for dynamic prompt block injection.
    /// When set, the system prompt includes blocks like heartbeat mode,
    /// skill matching, language, and platform — same as SubAgent.
    pub prompt_registry: Option<std::sync::Arc<crate::skills::prompt_registry::PromptRegistry>>,
    /// Sampling temperature for the proposer call. Default 0.2.
    pub temperature: f64,
    /// Output cap for proposer responses (mostly structured JSON, so small).
    pub max_tokens: Option<usize>,
    /// Optional independent advisor model. When set, the `consult_advisor`
    /// step routes its question to this model. When None, consult_advisor
    /// degrades gracefully (a hint is injected, no crash).
    pub advisor: Option<Arc<dyn Model>>,
    /// Max items per `explore` step. Default 1 — see `MAX_EXPLORE_ITEMS_PER_STEP`.
    pub max_explore_items_per_step: usize,
    /// Max chars per explore-item question. Default 2000.
    pub max_explore_question_chars: usize,
}

impl GraphProposer {
    pub fn new(
        model: Arc<dyn Model>,
        tools: Arc<ToolRegistry>,
        skills: Option<std::sync::Arc<dyn crate::skills::SkillStorage>>,
    ) -> Self {
        Self {
            model,
            tools,
            skills,
            prompt_registry: None,
            temperature: 0.2,
            max_tokens: Some(32768),
            advisor: None,
            max_explore_items_per_step: MAX_EXPLORE_ITEMS_PER_STEP,
            max_explore_question_chars: MAX_EXPLORE_QUESTION_CHARS,
        }
    }

    pub fn with_temperature(mut self, t: f64) -> Self {
        self.temperature = t;
        self
    }

    /// Attach an independent advisor model for the `consult_advisor` step.
    pub fn with_advisor(mut self, advisor: Arc<dyn Model>) -> Self {
        self.advisor = Some(advisor);
        self
    }

    /// Override the max_tokens cap for proposer calls. The default of 4096
    /// suits reasoning-style models on medium-complexity patches; bump
    /// higher if the model truncates large payloads mid-string.
    pub fn with_max_tokens(mut self, n: usize) -> Self {
        self.max_tokens = Some(n);
        self
    }

    /// Override the per-step explore-items cap. Default 1.
    pub fn with_max_explore_items(mut self, n: usize) -> Self {
        self.max_explore_items_per_step = n;
        self
    }

    /// Override the per-item explore-question char cap. Default 2000.
    pub fn with_max_explore_question_chars(mut self, n: usize) -> Self {
        self.max_explore_question_chars = n;
        self
    }

    /// Attach a skill storage. The Proposer will list available skills
    /// in its system prompt.
    pub fn with_skills(
        mut self,
        skills: std::sync::Arc<dyn crate::skills::SkillStorage>,
    ) -> Self {
        self.skills = Some(skills);
        self
    }

    /// Attach a PromptRegistry for dynamic block injection into the
    /// system prompt (heartbeat mode, skill matching, language, etc.).
    pub fn with_prompt_registry(
        mut self,
        pr: std::sync::Arc<crate::skills::prompt_registry::PromptRegistry>,
    ) -> Self {
        self.prompt_registry = Some(pr);
        self
    }

    /// Build the system prompt for a given task. Includes the schema for
    /// `ProposerStep` and the currently registered tools.
    ///
    /// Prompt sections are loaded from `skills/prompts/proposer-*.md` if
    /// those files exist, falling back to the hardcoded defaults below.
    /// Edit the .md files to tune prompts without recompiling.
    pub fn build_system_prompt(&self, task: &str) -> String {
        let mut tools_section = String::new();
        let defs = self.tools.defs();
        if defs.is_empty() {
            tools_section.push_str(
                "(no direct tools available to you — your only execution path is the `explore` step, \
                 which dispatches a subagent that has the actual tools. If you emit `call_tool` it will fail.)\n",
            );
        } else {
            for def in &defs {
                tools_section.push_str(&format!(
                    "- `{}` — {}\n  args schema: {}\n",
                    def.name,
                    def.description,
                    serde_json::to_string(&def.schema).unwrap_or_else(|_| "{}".into())
                ));
            }
        }

        // Load prompts from files if available, fall back to hardcoded.
        let preamble = load_prompt_file("skills/prompts/proposer-preamble.md", PROMPT_PREAMBLE);
        let iron_laws = load_prompt_file("skills/prompts/graph-centric-iron-laws.md", PROMPT_IRON_LAWS);
        let intake = load_prompt_file("skills/prompts/proposer-intake.md", PROMPT_INTAKE);
        let rules = load_prompt_file("skills/prompts/proposer-rules.md", PROMPT_RULES);

        let mut prompt = format!(
            "{preamble}\n\n{iron_laws}\n\n{intake}\n\n## Task\n{task}\n\n## Available Tools\n{tools_section}\n{rules}"
        );

        // Append the skills section if a storage is attached.
        if let Some(skills) = &self.skills {
            let section = crate::skills::retrieve::list_for_prompt(skills.as_ref());
            if !section.is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(&section);
            }
        }

        // v2.5: inject dynamic prompt blocks via PromptRegistry.
        // Same mechanism as SubAgent — heartbeat, skill matching,
        // language, platform all flow through the registry.
        if let Some(pr) = &self.prompt_registry {
            let ctx = crate::skills::prompt_registry::PromptContext {
                role: "edit".into(),
                task_description: task.to_string(),
                language: Some("Chinese".into()),
                is_heartbeat: false, // set by caller via build_system_prompt_for_heartbeat
                platform: if cfg!(target_os = "windows") {
                    "windows".into()
                } else {
                    "linux".into()
                },
                auto_apply_skills: true,
                matched_skills: String::new(),
                ..Default::default()
            };
            let dynamic = pr.compose(&[], &ctx);
            if !dynamic.is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(&dynamic);
            }
        }

        // v2.5: document skill node ID prefix.
        prompt.push_str("\n\n## Skill-Aware Task Graphs\n\
When a past skill is auto-matched to your current task, its compiled task \
graph is injected as `skill:<slug>:<node-id>` nodes into the task plan. \
These behave like regular Task nodes — you may add edges to or from them, \
re-plan them, or supplement them with additional nodes from the Decomposer.");

        prompt
    }

    /// Build a system prompt with heartbeat context. Sets `is_heartbeat: true`
    /// so the PromptRegistry injects the autonomous-mode block.
    pub fn build_system_prompt_heartbeat(&self, task: &str) -> String {
        let prompt = self.build_system_prompt(task);
        // Append heartbeat-specific override: no questions, direct execution.
        if let Some(pr) = &self.prompt_registry {
            let ctx = crate::skills::prompt_registry::PromptContext {
                role: "edit".into(),
                task_description: task.to_string(),
                language: Some("Chinese".into()),
                is_heartbeat: true,
                platform: if cfg!(target_os = "windows") {
                    "windows".into()
                } else {
                    "linux".into()
                },
                auto_apply_skills: true,
                matched_skills: String::new(),
                ..Default::default()
            };
            let _hb_block = pr.compose(&[], &ctx);
            // Replace the default (non-heartbeat) dynamic section with the
            // heartbeat-aware version.
            // Rebuild the whole prompt cleanly.
            return self.build_system_prompt_with_ctx(task, &ctx);
        }
        prompt
    }

    /// Internal: build system prompt with an explicit PromptContext.
    fn build_system_prompt_with_ctx(
        &self,
        task: &str,
        ctx: &crate::skills::prompt_registry::PromptContext,
    ) -> String {
        let mut tools_section = String::new();
        let defs = self.tools.defs();
        if defs.is_empty() {
            tools_section.push_str(
                "(no direct tools available to you — your only execution path is the `explore` step, \
                 which dispatches a subagent that has the actual tools. If you emit `call_tool` it will fail.)\n",
            );
        } else {
            for def in &defs {
                tools_section.push_str(&format!(
                    "- `{}` — {}\n  args schema: {}\n",
                    def.name, def.description,
                    serde_json::to_string(&def.schema).unwrap_or_else(|_| "{}".into())
                ));
            }
        }

        let preamble = load_prompt_file("skills/prompts/proposer-preamble.md", PROMPT_PREAMBLE);
        let iron_laws = load_prompt_file("skills/prompts/graph-centric-iron-laws.md", PROMPT_IRON_LAWS);
        let intake = load_prompt_file("skills/prompts/proposer-intake.md", PROMPT_INTAKE);
        let rules = load_prompt_file("skills/prompts/proposer-rules.md", PROMPT_RULES);

        let mut prompt = format!(
            "{preamble}\n\n{iron_laws}\n\n{intake}\n\n## Task\n{task}\n\n## Available Tools\n{tools_section}"
        );

        // Dynamic blocks from PromptRegistry (before PROMPT_RULES).
        if let Some(pr) = &self.prompt_registry {
            let dynamic = pr.compose(&[], ctx);
            if !dynamic.is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(&dynamic);
            }
        }

        prompt.push_str("\n");
        prompt.push_str(&rules);

        // Skills section.
        if let Some(skills) = &self.skills {
            let section = crate::skills::retrieve::list_for_prompt(skills.as_ref());
            if !section.is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(&section);
            }
        }

        // Skill-Aware note.
        prompt.push_str("\n\n## Skill-Aware Task Graphs\n\
When a past skill is auto-matched to your current task, its compiled task \
graph is injected as `skill:<slug>:<node-id>` nodes. These behave like \
regular Task nodes.");

        prompt
    }

    /// Compose a `Conversation` seeded with this proposer's system prompt
    /// and task. Convenience wrapper — callers may build the conversation
    /// themselves for finer control.
    pub fn make_conversation(&self, task: impl Into<String>) -> Conversation {
        let task = task.into();
        let system = self.build_system_prompt(&task);
        Conversation::new(system, task)
    }

    /// One step: ask the model what to do next, parse the structured reply.
    /// One step: ask the model what to do next, parse the structured
    /// reply. Returns the parsed step + the token usage for this call
    /// (so the caller can accumulate + surface cost).
    pub async fn next_step(
        &self,
        conv: &Conversation,
        graph: &Graph,
        prev_step: Option<&ProposerStep>,
    ) -> Result<(ProposerStep, u64)> {
        let snapshot = render_graph_for_prompt(graph);
        let mut req = conv.to_request(&snapshot, self.temperature, self.max_tokens);
        // Make sure the system prompt is consistent with this proposer's task.
        if let Some(first) = req
            .messages
            .iter_mut()
            .find(|m| matches!(m.role, Role::System))
        {
            let want = self.build_system_prompt(&conv.task_description);
            if first.content != want {
                first.content = want;
            }
        }
        // Enable native function calling (OpenAI tool_calls).
        req.tools = proposer_tools(self.advisor.is_some());

        let resp = self.model.complete(req).await?;
        let tokens = resp.usage.total_tokens as u64;
        debug!(
            content_len = resp.content.len(),
            tool_calls = resp.tool_calls.len(),
            tokens,
            "proposer received model response"
        );

        // Prefer native tool_calls (structured, no JSON escape issues).
        let step = if !resp.tool_calls.is_empty() {
            parse_step_from_tool_calls(&resp.tool_calls)?
        } else {
            // DeepSeek / M3 reasoning models put their final JSON in
            // reasoning_content, leaving content empty. See
            // ModelResponse::text_or_reasoning and commit 7b8322e.
            let parse_text = resp.text_or_reasoning();
            if parse_text.trim().is_empty() {
                return Err(HarnessError::model(
                    "proposer: empty response — model returned neither tool_calls, content, nor reasoning_content"
                ));
            }
            parse_step(parse_text, self.max_explore_items_per_step, self.max_explore_question_chars)?
        };

        // Layer 3: post-Explore commit gate. After an Explore step the
        // next step MUST be a graph-committing or pause-declaring step
        // (ProposePatch / Block / AskUser / ReadyForVerify) — never
        // another Explore (or CallTool that bypasses the graph). Without
        // this, the model keeps dispatching subagents and never updates
        // the graph, which is the 602-round infinite-explore failure
        // mode we hit in production 2026-06-09. The error message is
        // picked up by the fix-it retry path so the model gets a
        // second chance to commit before the run dies.
        if let Some(ProposerStep::Explore { .. }) = prev_step {
            let ok = matches!(
                step,
                ProposerStep::ProposePatch { .. }
                    | ProposerStep::Block { .. }
                    | ProposerStep::AskUser { .. }
                    | ProposerStep::ReadyForVerify { .. }
            );
            if !ok {
                return Err(HarnessError::model(format!(
                    "post-Explore commit gate: your previous step was an Explore \
                     (subagent finished and reported back). The next step must be \
                     one of: \
                     1) `propose_patch` — commit the subagent's findings to the \
                        graph (add nodes/edges for what you learned), \
                     2) `block` — declare you're stuck on something the user must \
                        resolve, \
                     3) `ask_user` — ask the user a clarifying question, \
                     4) `ready_for_verify` — declare the graph is complete. \
                     Dispatching another Explore (or any non-committing step) \
                     without first committing the previous subagent's findings \
                     is a violation. Got step kind: {}",
                    step.kind()
                )));
            }
        }

        Ok((step, tokens))
    }

    /// Same as [`Self::next_step`] but retries the model call once if
    /// the first response is malformed. If both attempts fail, salvages
    /// a best-effort step from the response text instead of dying.
    pub async fn next_step_with_retry(
        &self,
        conv: &Conversation,
        graph: &Graph,
        prev_step: Option<&ProposerStep>,
    ) -> Result<(ProposerStep, u64)> {
        let mut total_tokens: u64 = 0;
        let (step, parse_err) = match self.next_step(conv, graph, prev_step).await {
            Ok((s, t)) => {
                total_tokens += t;
                (s, None)
            }
            Err(e) => (ProposerStep::ReadyForVerify { rationale: String::new() }, Some(e)),
        };

        // Layer 1: parse-error retry.
        if let Some(parse_err) = parse_err {
            warn!(
                error = %parse_err,
                "proposer first response was malformed; retrying once with a fix-it prompt"
            );
            let mut patched_conv = conv.clone();
            patched_conv.messages.push(crate::model::Message {
                role: Role::User,
                content: format!(
                    "Your previous response was malformed (parser said: {parse_err}). \
                     Reply with EXACTLY one valid JSON object matching one of the step \
                     schemas above. No markdown fences, no prose around it."
                ),
            });
            match self.next_step(&patched_conv, graph, prev_step).await {
                Ok((s, t)) => return Ok((s, total_tokens + t)),
                Err(retry_err) => {
                    // Both attempts failed to produce valid JSON. Salvage:
                    // treat this as the model trying to communicate and
                    // surface its prose as an `ask_user` step. For
                    // unattended/heartbeat runs this gets auto-answered
                    // with "proceed", giving the model a natural retry in
                    // the next round without dying.
                    warn!(
                        first = %parse_err,
                        retry = %retry_err,
                        "proposer: both attempts failed; salvaging as ask_user fallback"
                    );
                    let question = extract_salvage_question(&parse_err.to_string());
                    return Ok((
                        ProposerStep::AskUser {
                            question,
                            options: vec![],
                            rationale: format!(
                                "Model did not produce valid JSON after retry. \
                                 First error: {parse_err}. Retry error: {retry_err}. \
                                 Falling back to ask_user to keep the loop alive."
                            ),
                        },
                        total_tokens,
                    ));
                }
            };
        }

        // Intake gate (softened 2026-06-06): the system prompt still
        // teaches the Mode A / Mode B rule, but the runtime no longer
        // blocks when the model returns `explore` (or anything other
        // than `ask_user`) on a vague task. We log the violation for
        // visibility, then let the step through — refusing to make
        // progress because the model didn't ask a question was worse
        // than letting it read first and decide. A "propose_patch on
        // a vague task" is still surfaced through this log line; the
        // human can intervene in chat.
        if let Err(intake_err) = crate::agent::intake::check_intake_compliance(
            &conv.task_description,
            conv.round,
            &step,
        ) {
            warn!(
                error = %intake_err,
                "intake: vague task first step was not ask_user; allowing it through"
            );
        }

        Ok((step, total_tokens))
    }

    /// Called when a sub-agent reports that a node failed execution.
    /// The Proposer re-plans the failed node and its downstream path,
    /// producing a GraphPatch.
    pub async fn replan_failed_node(
        &self,
        failed_node: &NodeId,
        error_evidence: &str,
        graph: &Graph,
        task: &str,
        conversation: &Conversation,
    ) -> Result<GraphPatch> {
        let graph_snapshot = render_graph_for_prompt(graph);
        let prompt = format!(
            r#"You are re-planning a failed node in a task graph.

## Original Task
{task}

## Current Graph
{graph_snapshot}

## Failed Node
Node ID: {failed_node}
Failure Evidence: {error_evidence}

## Instructions
The sub-agent attempted to execute this node and failed.
Your job is to:
1. Analyze WHY the node failed (from the evidence)
2. Design a REPLACEMENT for this node that avoids the failure
3. If the replacement changes this node's output contract, also adjust
   downstream nodes that depend on it
4. Output a GraphPatch with:
   - remove_nodes: the failed node's ID (and any dependent nodes that
     must change)
   - add_nodes: replacement node(s) with L0 (id, kind, path, summary)
   - add_edges: edges connecting new node(s) to existing nodes

Respond with JSON:
{{"step":"propose_patch","patch":{{"remove_nodes":[...],"add_nodes":[...],"add_edges":[...],"reason":"..."}},"rationale":"..."}}"#,
            failed_node = failed_node.as_str(),
        );

        let req = crate::model::ModelRequest {
            messages: {
                let mut msgs = conversation.messages.clone();
                msgs.push(Message::user(prompt));
                msgs
            },
            tools: vec![],
            temperature: 0.1,
            max_tokens: Some(32768),
            stop: vec![],
        };

        let resp = self.model.complete(req).await?;
        let step = match parse_step(&resp.content, self.max_explore_items_per_step, self.max_explore_question_chars) {
            Ok(s) => s,
            Err(e) => {
                // Salvage: model returned prose instead of JSON.
                // Return an empty patch — the caller (cascade) will
                // see no changes and surface the stalemate naturally.
                warn!(error = %e, "replan: parse_step failed, returning empty patch");
                return Ok(GraphPatch::default());
            }
        };
        match step {
            ProposerStep::ProposePatch { patch, .. } => Ok(patch),
            other => {
                warn!(step = other.kind(), "replan: expected propose_patch, returning empty patch");
                Ok(GraphPatch::default())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering the graph for the prompt
// ---------------------------------------------------------------------------

/// Render the graph compactly for inclusion in the model's prompt. Different
/// from `context::render_local_graph` in that it uses a more JSON-friendly,
/// idempotent layout the model is more likely to reason cleanly about.
///
/// Includes L1 summaries inline next to each node so the model always sees
/// the current semantic state of the graph alongside the structure.
/// Render the graph compactly for inclusion in the model's prompt. Different
/// from `context::render_local_graph` in that it uses a more JSON-friendly,
/// idempotent layout the model is more likely to reason cleanly about.
///
/// Includes L1 summaries inline next to each node so the model always sees
/// the current semantic state of the graph alongside the structure.
pub(crate) fn render_graph_for_prompt(g: &Graph) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "graph version={} status={:?} nodes={} edges={} l1_entries={}\n",
        g.version,
        g.status,
        g.node_count(),
        g.edge_count(),
        g.l1.len(),
    ));
    let mut node_ids: Vec<&NodeId> = g.nodes.keys().collect();
    node_ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    if !node_ids.is_empty() {
        s.push_str("nodes (L0 + L1 oneline):\n");
        for id in node_ids {
            if let Some(n) = g.get_node(id) {
                let l1_hint = g
                    .l1
                    .get(id)
                    .filter(|d| !d.is_blank())
                    .map(|d| format!(" L1=\"{}\" (c={:.2})", d.render_oneline(), d.confidence))
                    .unwrap_or_else(|| " L1=(not yet enriched)".to_string());
                s.push_str(&format!(
                    "  - id={} kind={:?} summary={:?}{}\n",
                    n.id, n.kind, n.summary, l1_hint
                ));
            }
        }
    }
    if g.edge_count() > 0 {
        s.push_str("edges:\n");
        for (i, e) in g.iter_edges().enumerate() {
            s.push_str(&format!(
                "  [{i}] {} -[{:?} c={:.2}]-> {}  evidence={:?}\n",
                e.source, e.relation, e.confidence, e.target, e.evidence
            ));
        }
    }
    s
}

// ---------------------------------------------------------------------------
// JSON extraction + parsing
// ---------------------------------------------------------------------------

/// Strip `<think>...</think>` blocks from a model response.
/// Modern reasoning models (DeepSeek-v3, MiniMax-M3, Claude with
/// extended thinking) emit a chain-of-thought block BEFORE the
/// actual answer; if we don't strip it, our `find('{')` lands
/// inside the think text and the JSON parse blows up on the first
/// `}` that closes the think reasoning rather than a real JSON
/// value. Strip the think first, then look for the JSON.
fn strip_think_blocks(text: &str) -> String {
    // Match `<think>...</think>` (case-insensitive, lazy match, dot-matches-newline).
    // `regex` 1.x has no backreferences, so we have to do `<think>`
    // and `</think>` as separate passes. Each pass strips both the
    // opening and the closing tag if they're paired.
    static OPEN: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?is)<\s*think\s*>").expect("think-open regex")
    });
    static CLOSE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?is)</\s*think\s*>").expect("think-close regex")
    });
    // Walk: find the first `<think...>`, find the matching
    // `</think...>`, drop the slice. Repeat for nested/multi-pass
    // cases. This is O(n*passes) but pass count is bounded.
    let mut s = text.to_string();
    loop {
        let open = match OPEN.find(&s) {
            Some(m) => m.start()..m.end(),
            None => break,
        };
        // After the open tag, find the next close.
        let close_start = match CLOSE.find(&s[open.end..]) {
            Some(m) => open.end + m.start(),
            None => break, // unterminated — leave the rest alone
        };
        let close_end = close_start + CLOSE.find(&s[close_start..]).unwrap().len();
        s = format!("{}{}", &s[..open.start], &s[close_end..]);
    }
    s
}

/// Pull the first **valid** JSON object out of a (possibly markdown-wrapped)
/// model response. Tolerant of leading prose, embedded examples in reasoning
/// blocks, code-fence variants, and `<think>...</think>` tags.
///
/// We find every *outermost* `{` (brace depth 0), balance-walk to its `}`,
/// then try each candidate right-to-left — the last JSON block is usually
/// the real one. Models with or without thinking/reasoning work regardless
/// of whether the thinking contains example-JSON snippets.
pub fn extract_json_block(text: &str) -> Result<String> {
    let trimmed = strip_think_blocks(text).trim().to_string();
    // Strip ```json ... ``` or ``` ... ``` fences.
    let inner: &str = if let Some(rest) = trimmed.strip_prefix("```json") {
        rest.trim_start_matches('\n')
            .rsplit_once("```")
            .map(|(a, _)| a)
            .unwrap_or(rest)
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest.trim_start_matches('\n')
            .rsplit_once("```")
            .map(|(a, _)| a)
            .unwrap_or(rest)
    } else {
        &trimmed
    };

    // Collect every *outermost* `{` (brace depth 0) with its balanced close.
    let chars: Vec<char> = inner.chars().collect();
    let char_indices: Vec<usize> = inner.char_indices().map(|(i, _)| i).collect();
    let mut candidates: Vec<(usize, usize)> = Vec::new(); // (start_byte, end_byte)

    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '{' {
            // Walk to balanced close.
            let mut depth = 1i32;
            let mut in_string = false;
            let mut escape = false;
            let mut end: Option<usize> = None;
            for j in (i + 1)..chars.len() {
                let c = chars[j];
                if escape { escape = false; continue; }
                if in_string {
                    match c { '\\' => escape = true, '"' => in_string = false, _ => {} }
                    continue;
                }
                match c {
                    '"' => in_string = true,
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 { end = Some(j); break; }
                    }
                    _ => {}
                }
            }
            if let Some(end_idx) = end {
                let start_byte = char_indices[i];
                let end_byte = char_indices[end_idx] + chars[end_idx].len_utf8();
                candidates.push((start_byte, end_byte));
                i = end_idx + 1; // skip past this balanced block
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    if candidates.is_empty() {
        return Err(HarnessError::model(format!(
            "proposer: no '{{' in response; raw={:?}",
            text.chars().take(500).collect::<String>()
        )));
    }

    // Try right-to-left: the last outermost block is usually the real response.
    let mut last_err: Option<String> = None;
    for (start, end) in candidates.iter().rev() {
        let candidate = &inner[*start..*end];
        if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
            return Ok(candidate.to_string());
        }
        last_err = Some(format!(
            "JSON parse failed for outermost block at byte {start}: {}",
            serde_json::from_str::<serde_json::Value>(candidate).unwrap_err()
        ));
    }

    Err(HarnessError::model(format!(
        "proposer: no valid outermost JSON found. Last error: {}",
        last_err.unwrap_or_else(|| "no candidates".into())
    )))
}

/// Extract a meaningful question from a model's prose response that
/// lacked any JSON. Used as a last-resort salvage to keep the loop alive.
fn extract_salvage_question(parse_error: &str) -> String {
    // If the error mentions the model's raw text, use a fragment of it.
    // Otherwise, generate a generic continuation prompt.
    let default_q = "Continue working on the task. Please output a valid JSON step this time.";
    // Try to extract something useful from the error — it often includes
    // the first ~100 chars of the raw response in "invalid JSON: ...".
    if let Some(raw) = parse_error.split("--- raw ---").nth(1) {
        let snippet: String = raw.chars().take(200).collect();
        if !snippet.trim().is_empty() {
            return format!("The model said: \"{}\" — how should we proceed?", snippet.trim());
        }
    }
    default_q.to_string()
}

fn proposer_tools(has_advisor: bool) -> Vec<serde_json::Value> {
    let mut tools = vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "propose_patch",
                "description": "Add/remove nodes and edges on the relationship graph. Use this for ALL graph modifications. The graph flows start → deliverable: `start` is the immutable anchor (current state), `deliverable` is the goal, and intermediate step nodes go BETWEEN them. Example minimal seed: {\"patch\":{\"add_nodes\":[{\"id\":\"start\",\"kind\":\"Task\",\"summary\":\"Start: current state\",\"immutable\":true},{\"id\":\"deliverable\",\"kind\":\"Task\",\"summary\":\"Deliverable: desired outcome\"}],\"add_edges\":[{\"source\":\"start\",\"target\":\"deliverable\",\"relation\":\"LeadsTo\",\"confidence\":0.9}],\"reason\":\"seed start and deliverable\"},\"rationale\":\"establish start and deliverable\"}",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "patch": {
                            "type": "object",
                            "properties": {
                                "add_nodes": {
                                    "type": "array",
                                    "description": "Nodes to add. Each node: id (string, required), kind (one of File/Function/Class/Module/Config/Task/Other), summary (string, what it is), and optional path, immutable (bool), expanded (bool).",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "id": {"type": "string"},
                                            "kind": {"type": "string", "enum": ["File", "Function", "Class", "Module", "Config", "Task", "Other"]},
                                            "summary": {"type": "string"},
                                            "path": {"type": "string"},
                                            "immutable": {"type": "boolean"},
                                            "expanded": {"type": "boolean"}
                                        },
                                        "required": ["id", "kind", "summary"]
                                    }
                                },
                                "add_edges": {
                                    "type": "array",
                                    "description": "Edges to add. Each edge: source (node id, required), target (node id, required), relation (LeadsTo for process flow / sequencing — the start→deliverable main chain and most step-to-step edges use this; DependsOn for true dependencies; Contains for hierarchy; or Imports/Exports/Calls/Triggers/Reads/Writes/Other), confidence (0..1). Edges flow source→target (source leads to / feeds target).",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "source": {"type": "string"},
                                            "target": {"type": "string"},
                                            "relation": {"type": "string", "enum": ["LeadsTo", "DependsOn", "Contains", "BelongsTo", "Imports", "Exports", "Calls", "Triggers", "Reads", "Writes", "Other"]},
                                            "confidence": {"type": "number"},
                                            "evidence": {"type": "string"}
                                        },
                                        "required": ["source", "target", "relation"]
                                    }
                                },
                                "remove_node_ids": {"type": "array", "items": {"type": "string"}},
                                "remove_edge_indices": {"type": "array", "items": {"type": "integer"}},
                                "set_l1": {"type": "object"},
                                "reason": {"type": "string"}
                            },
                            "required": ["reason"]
                        },
                        "rationale": {"type": "string"}
                    },
                    "required": ["patch", "rationale"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "explore",
                "description": "Dispatch a subagent to read files and search the codebase. Use this to gather information before modifying the graph.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "items": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "scope": {"type": "string", "description": "Directory, file glob, or URL to explore"},
                                    "question": {"type": "string", "description": "Specific question the subagent should answer"}
                                },
                                "required": ["scope", "question"]
                            }
                        },
                        "rationale": {"type": "string"}
                    },
                    "required": ["items", "rationale"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "ask_user",
                "description": "Ask the user a clarifying question. During the goal-clarification phase, state your current understanding of the goal, then provide `options` (a few concrete choices the user can pick — the user can always also reply with their own answer). Use this to confirm the goal before building the graph.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "question": {"type": "string"},
                        "options": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "A few concrete choices for the user to pick from. The user may also type their own answer. Provide 2-4 options when clarifying a goal."
                        },
                        "rationale": {"type": "string"}
                    },
                    "required": ["question", "rationale"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "ready_for_verify",
                "description": "Declare the graph complete and hand off to verification.",
                "parameters": {
                    "type": "object",
                    "properties": { "rationale": {"type": "string"} },
                    "required": ["rationale"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "block",
                "description": "Pause and request specific input from the user (credentials, UX choice, etc). Only use when truly blocked.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "reason": {"type": "string", "description": "Short label of what you're blocked on"},
                        "needed_from_user": {"type": "string", "description": "What the user should provide"},
                        "rationale": {"type": "string"}
                    },
                    "required": ["reason", "rationale"]
                }
            }
        }),
    ];
    if has_advisor {
        tools.push(serde_json::json!({
            "type": "function",
            "function": {
                "name": "consult_advisor",
                "description": "Ask the independent advisor model a design or knowledge question and get a second opinion. The advisor only answers — it does NOT modify the graph. Use when you're genuinely unsure how to proceed and a second perspective would help. The answer is added to the conversation; you then decide the next step.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "question": {"type": "string", "description": "The question for the advisor"},
                        "context": {"type": "string", "description": "Optional relevant context: what you've tried, constraints, current graph state"},
                        "rationale": {"type": "string"}
                    },
                    "required": ["question", "rationale"]
                }
            }
        }));
    }
    tools
}

/// Parse a ProposerStep from native OpenAI tool_calls.
fn parse_step_from_tool_calls(tool_calls: &[crate::model::ToolCall]) -> Result<ProposerStep> {
    let tc = &tool_calls[0];
    match tc.name.as_str() {
        "propose_patch" => {
            // Route through the tolerant `parse_patch` (same as the text
            // path) instead of strict serde deserialization. Strict
            // deserialization rejected patches missing optional fields
            // (e.g. "missing field `add_nodes`") — which any model that
            // omits an empty array would trigger. The tolerant parser
            // treats every field as optional and maps common aliases.
            let patch_val = tc.arguments.get("patch").unwrap_or(&tc.arguments);
            let patch = parse_patch(patch_val)?;
            validate_drill_down(&patch)?;
            Ok(ProposerStep::ProposePatch {
                patch,
                rationale: tc.arguments.get("rationale").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            })
        }
        "explore" => {
            let items: Vec<ExploreItem> = tc.arguments
                .get("items")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|item| {
                    Some(ExploreItem {
                        scope: item.get("scope")?.as_str()?.to_string(),
                        question: item.get("question")?.as_str()?.to_string(),
                    })
                }).collect())
                .unwrap_or_default();
            let rationale = tc.arguments.get("rationale").and_then(|v| v.as_str()).unwrap_or("").to_string();
            Ok(ProposerStep::Explore { items, rationale })
        }
        "ask_user" => {
            let question = tc.arguments.get("question").and_then(|v| v.as_str()).unwrap_or("").to_string();
            // Extract structured options from tool_calls.
            let options: Vec<String> = tc
                .arguments
                .get("options")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|o| o.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            // Also append to question for non-UI consumers.
            let mut q = question;
            if !options.is_empty() {
                q.push_str("\n\nOptions:");
                for (i, o) in options.iter().enumerate() {
                    q.push_str(&format!("\n  {}. {}", i + 1, o));
                }
                q.push_str("\n\nReply with a number, or type your own answer.");
            }
            Ok(ProposerStep::AskUser {
                question: q,
                options,
                rationale: tc.arguments.get("rationale").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            })
        }
        "ready_for_verify" => {
            Ok(ProposerStep::ReadyForVerify {
                rationale: tc.arguments.get("rationale").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            })
        }
        "block" => {
            Ok(ProposerStep::Block {
                reason: tc.arguments.get("reason").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                needed_from_user: tc.arguments.get("needed_from_user").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                rationale: tc.arguments.get("rationale").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            })
        }
        "consult_advisor" => {
            Ok(ProposerStep::ConsultAdvisor {
                question: tc.arguments.get("question").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                context: tc.arguments.get("context").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                rationale: tc.arguments.get("rationale").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            })
        }
        other => Err(HarnessError::model(format!("unknown tool_call: {other}"))),
    }
}

pub fn parse_step(
    text: &str,
    max_explore_items: usize,
    max_explore_question_chars: usize,
) -> Result<ProposerStep> {
    let cleaned = extract_json_block(text)?;
    let value: serde_json::Value = serde_json::from_str(&cleaned).map_err(|e| {
        HarnessError::model(format!(
            "proposer: invalid JSON: {e}\n--- raw ---\n{text}\n--- cleaned ---\n{cleaned}"
        ))
    })?;

    let step = value
        .get("step")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::model("proposer: missing 'step' field".to_string()))?;
    let rationale = value
        .get("rationale")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    match step {
        "ask_user" => {
            let question = value
                .get("question")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    HarnessError::model("proposer: ask_user requires 'question'".to_string())
                })?
                .to_string();
            let options: Vec<String> = value
                .get("options")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|o| o.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            Ok(ProposerStep::AskUser { question, options, rationale })
        }
        "call_tool" => {
            let tool = value
                .get("tool")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    HarnessError::model("proposer: call_tool requires 'tool'".to_string())
                })?
                .to_string();
            let args = value
                .get("args")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            Ok(ProposerStep::CallTool {
                tool,
                args,
                rationale,
            })
        }
        "propose_patch" => {
            let patch_v = value.get("patch").ok_or_else(|| {
                HarnessError::model("proposer: propose_patch requires 'patch'".to_string())
            })?;
            let patch = parse_patch(patch_v)?;
            validate_drill_down(&patch)?;
            Ok(ProposerStep::ProposePatch { patch, rationale })
        }
        "ready_for_verify" => Ok(ProposerStep::ReadyForVerify { rationale }),
        "block" => {
            let reason = value
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let needed_from_user = value
                .get("needed_from_user")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(ProposerStep::Block {
                reason,
                needed_from_user,
                rationale,
            })
        }
        "explore" => {
            // Accept both the new multi-item shape
            //   {"items": [{"scope":..., "question":...}, ...]}
            // and the legacy single-item shape
            //   {"scope":..., "question":...}
            // (folded into a 1-element vec) for backward compat.
            let items: Vec<ExploreItem> = if let Some(arr) =
                value.get("items").and_then(|v| v.as_array())
            {
                // Hard cap on items per step. Above this the JSON gets
                // too long for the model to keep well-formed (we hit
                // `invalid JSON: expected ',' or '}' at line N col M`
                // on a 6-item step in production 2026-06-06). The
                // fix-it retry path surfaces this back to the model,
                // which splits into two `Explore` steps.
                if arr.len() > max_explore_items {
                    return Err(HarnessError::model(format!(
                        "proposer: explore items[] has {} entries; the cap is \
                         {}. Split into two `Explore` steps with fewer items each.",
                        arr.len(),
                        max_explore_items
                    )));
                }
                let mut out = Vec::with_capacity(arr.len());
                for (i, item) in arr.iter().enumerate() {
                    let scope = item
                        .get("scope")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let question = item
                        .get("question")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if scope.is_empty() || question.is_empty() {
                        return Err(HarnessError::model(format!(
                            "proposer: explore items[{i}] has empty scope or question"
                        )));
                    }
                    // Hard cap on per-item question length. Long
                    // questions (1k+ chars) are where the model loses
                    // JSON well-formedness — quote escaping, line
                    // continuation, etc. Split into multiple focused
                    // items instead.
                    if question.len() > max_explore_question_chars {
                        return Err(HarnessError::model(format!(
                            "proposer: explore items[{i}] question is {} chars; \
                             cap is {}. Split the question into multiple \
                             `Explore` items, each with a focused question.",
                            question.len(),
                            max_explore_question_chars
                        )));
                    }
                    out.push(ExploreItem { scope, question });
                }
                out
            } else {
                // Legacy: single scope + question
                let scope = value
                    .get("scope")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let question = value
                    .get("question")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if scope.is_empty() || question.is_empty() {
                    return Err(HarnessError::model(
                        "proposer: explore requires non-empty 'scope' and 'question' (or 'items')"
                            .to_string(),
                    ));
                }
                vec![ExploreItem { scope, question }]
            };
            if items.is_empty() {
                return Err(HarnessError::model(
                    "proposer: explore items[] is empty (need at least one item)".to_string(),
                ));
            }
            Ok(ProposerStep::Explore { items, rationale })
        }
        "consult_advisor" => {
            let question = value
                .get("question")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    HarnessError::model("proposer: consult_advisor requires 'question'".to_string())
                })?
                .to_string();
            let context = value
                .get("context")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(ProposerStep::ConsultAdvisor { question, context, rationale })
        }
        other => Err(HarnessError::model(format!(
            "proposer: unknown step '{other}'"
        ))),
    }
}

fn parse_patch(v: &serde_json::Value) -> Result<GraphPatch> {
    let obj = v
        .as_object()
        .ok_or_else(|| HarnessError::model("proposer: patch must be an object".to_string()))?;

    let mut patch = GraphPatch::default();
    patch.reason = obj
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if let Some(arr) = obj.get("add_nodes").and_then(|v| v.as_array()) {
        for n in arr {
            patch.add_nodes.push(parse_node(n)?);
        }
    }
    if let Some(arr) = obj.get("add_edges").and_then(|v| v.as_array()) {
        for e in arr {
            patch.add_edges.push(parse_edge(e)?);
        }
    }
    if let Some(arr) = obj
        .get("remove_node_ids")
        .or_else(|| obj.get("remove_nodes"))
        .and_then(|v| v.as_array())
    {
        for id in arr {
            if let Some(s) = id.as_str() {
                patch.remove_node_ids.push(NodeId::from(s));
            }
        }
    }
    if let Some(arr) = obj.get("remove_edge_indices").and_then(|v| v.as_array()) {
        for i in arr {
            if let Some(idx) = i.as_u64() {
                patch.remove_edge_indices.push(idx as usize);
            }
        }
    }
    // drill_down (Task 2 follow-up): previously dead code — `parse_patch`
    // ignored the field, so the validator from the prior commit never saw
    // a model-emitted `drill_down` and the field was always `None` on the
    // produced GraphPatch. Now we read it; missing/null → None (most
    // patches), present and well-formed → Some(mark), present but
    // malformed → None with a warn log so a single bad field doesn't
    // drop the rest of the patch.
    match obj.get("drill_down") {
        None | Some(serde_json::Value::Null) => {
            // No drill_down marker — the default.
        }
        Some(v) => {
            match serde_json::from_value::<DrillDownMark>(v.clone()) {
                Ok(mark) => patch.drill_down = Some(mark),
                Err(e) => {
                    warn!(
                        error = %e,
                        raw = %v,
                        "proposer: patch.drill_down malformed; leaving field as None"
                    );
                }
            }
        }
    }
    Ok(patch)
}

/// Reject `drill_down` whose target is not present in the same patch's
/// `add_nodes`. The drill-down sub-graph machinery (see
/// `docs/superpowers/specs/2026-06-25-drill-down-sub-graph-design.md`)
/// forks a child GraphLoop for the target node — but it can only do so
/// if the parent patch actually added that node. A model that points
/// `drill_down.target` at an existing node (or a typo) would otherwise
/// silently drop the field, which is exactly the kind of "stuck because
/// a marker was silently swallowed" failure this validation prevents.
///
/// Returns `Err` (a model error) when validation fails so the caller can
/// surface it to the model via the fix-it retry path.
fn validate_drill_down(patch: &GraphPatch) -> Result<()> {
    if let Some(dd) = &patch.drill_down {
        if !patch.add_nodes.iter().any(|n| n.id == dd.target) {
            return Err(HarnessError::model(format!(
                "proposer: drill_down.target '{}' not in add_nodes; drill_down must target a node added in the same patch",
                dd.target.as_str()
            )));
        }
    }
    Ok(())
}

fn parse_node(v: &serde_json::Value) -> Result<Node> {
    let id = v
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::model("proposer: node missing 'id'".to_string()))?;
    let kind_str = v.get("kind").and_then(|v| v.as_str()).unwrap_or("Other");
    let kind = parse_node_kind(kind_str);
    let path = v.get("path").and_then(|v| v.as_str()).unwrap_or(id).to_string();
    let summary = v
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut node = Node::new(id, kind, path, summary);
    node.immutable = v
        .get("immutable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    node.expanded = v
        .get("expanded")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Some(meta) = v.get("metadata").and_then(|v| v.as_object()) {
        for (k, val) in meta {
            node = node.with_metadata(k, val.clone());
        }
    }
    Ok(node)
}

fn parse_node_kind(s: &str) -> NodeKind {
    match s {
        "File" => NodeKind::File,
        "Function" => NodeKind::Function,
        "Class" => NodeKind::Class,
        "Module" => NodeKind::Module,
        "Config" => NodeKind::Config,
        "Task" => NodeKind::Task,
        other => NodeKind::Other(other.to_string()),
    }
}

fn parse_edge(v: &serde_json::Value) -> Result<Edge> {
    // Accept common aliases: some models emit `from`/`to` instead of
    // `source`/`target`.
    let source = v
        .get("source")
        .or_else(|| v.get("from"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::model("proposer: edge missing 'source'".to_string()))?;
    let target = v
        .get("target")
        .or_else(|| v.get("to"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::model("proposer: edge missing 'target'".to_string()))?;
    let relation_str = v
        .get("relation")
        .or_else(|| v.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("Other");
    let relation = parse_relation_type(relation_str);
    let confidence = v
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5);
    let evidence = v
        .get("evidence")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(Edge::new(source, target, relation, confidence, evidence))
}

fn parse_relation_type(s: &str) -> RelationType {
    match s {
        "Contains" => RelationType::Contains,
        "BelongsTo" => RelationType::BelongsTo,
        "Imports" => RelationType::Imports,
        "Exports" => RelationType::Exports,
        "DependsOn" => RelationType::DependsOn,
        "LeadsTo" => RelationType::LeadsTo,
        "Calls" => RelationType::Calls,
        "Triggers" => RelationType::Triggers,
        "Reads" => RelationType::Reads,
        "Writes" => RelationType::Writes,
        "RevealedBy" => RelationType::RevealedBy,
        "InvalidatedBy" => RelationType::InvalidatedBy,
        other => RelationType::Other(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Prompt text — written for any modern instruction-tuned model.
// ---------------------------------------------------------------------------

/// Try to load a prompt from a file, falling back to the hardcoded default.
/// This lets users edit `skills/prompts/proposer-*.md` without recompiling.
fn load_prompt_file(path: &str, default: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| default.to_string())
}

const PROMPT_PREAMBLE: &str =
    "You are a Graph-Centric agent. Your job is to build a *relationship graph* \
that captures the user's task: its entities, their structural relationships, \
and the relevant constraints. The graph is the shared substrate between \
you, the user, and any sub-agents you might dispatch later.\n\
\n\
## The graph has three layers\n\
\n\
- **L0** (skeleton) — nodes + edges. What entities exist and how they relate. \
This is what your patches touch directly.\n\
- **L1** (muscle) — per-node semantic description: responsibility, \
implementation, design intent, constraints. You DO NOT write L1 yourself; an \
L1Enricher runs automatically after your patches and reads source/data (L2) \
to produce L1. You just focus on getting L0 right.\n\
- **L2** (skin) — the actual content: source files, configs, schemas, raw \
data. Accessed on demand via tools (e.g. `bash` with `cat`); never embedded \
in your patches.\n\
\n\
You operate in a strict iterative loop:\n\
  input → reason → extend graph → verify → (repair if wrong) → dispatch\n\
\n\
Each turn, you emit exactly ONE structured step as your entire response. \
The runtime executes the step (asking the user, invoking a tool, applying \
the patch, or transitioning to verification) and then asks you for the next \
step. Many small steps are better than few large ones — each step is also \
an opportunity to catch and reverse a mistake.\n\
\n\
## Anchor + Goal (A → D)\n\
\n\
Every task has a starting point (anchor A) and a desired outcome (goal D). \
**Your first patches MUST establish both A and D as explicit graph nodes** \
before filling in the intermediate nodes.\n\
\n\
- **Anchor A**: the user's immutable intent. Mark the anchor node with \
`\"immutable\": true`. Example: for \"analyze CodeWhale's architecture\", \
A is a Task node `task:analyze_codewhale`.\n\
- **Goal D**: what the user wants at the end. A deliverable node — a \
report, a code change, an answer, a deployment. Mark it as \
`kind: \"Task\"` and describe the expected output in its summary.\n\
\n\
If the goal D is unclear from the task, **ask the user to clarify** \
(use `ask_user`) BEFORE creating any intermediate nodes. It is always \
cheaper to ask one clarification question than to build a graph toward \
the wrong goal.\n\
\n\
**Building order:**\n\
1. First patch: create `start` (immutable anchor, current state) + `deliverable` (goal). Add a `LeadsTo` edge start→deliverable.\n\
2. Second patch (after exploration if needed): add intermediate step nodes \
BETWEEN start and deliverable. Connect them along the flow with `LeadsTo` \
(process/sequence); use `DependsOn` only for true dependencies, `Contains` for hierarchy.\n\
3. When all intermediate nodes are filled and verified, emit `ready_for_verify`.\n\
\n\
This start→deliverable flow gives the verifier a concrete convergence criterion: \
the graph is complete when there is a filled path of structural edges from start to deliverable.";

/// Intake rule — Mode A vs Mode B. The model's FIRST step in a fresh
/// conversation is an intake decision: clear task → propose_patch,
/// vague task → ask_user. Vague tasks are dangerous because the rest
/// of the loop (verifier, sub-agents) all see the first graph; a wrong
/// first interpretation has no recovery path inside a 24-round Graph
/// phase, so the cost of asking one targeted question is much lower
/// than the cost of guessing wrong.
const PROMPT_IRON_LAWS: &str = r#"## Graph-Centric Iron Laws

These laws override local convenience and model habits:

1. The relationship graph is the task's authoritative state. Do not rely on transcript memory when the graph should carry the fact.
2. The first graph for a fresh task has only A and D: A is the immutable anchor/current state, D is the desired verified result, and D DependsOn A.
3. Intermediate nodes are filled only after A/D exists. If you know the path, add steps. If you do not know the path, Explore first and convert evidence into graph nodes/edges.
4. Complex or abstract nodes must be recursively treated as their own A/D problem until they are concrete enough to execute.
5. Execution follows the graph. Inputs, outputs, evidence, and failures must be reflected in the graph or execution ledger, not just prose.
6. When a node fails, re-plan that node, then re-verify from the top-level A/D graph.
7. If the failed node failed because the previous node's output contract is wrong, re-plan the previous dependency and re-verify from the top-level A/D graph.
8. Try alternatives one at a time. Do not front-load a complete enumeration of every possible plan.
9. Never remove or rewrite the anchor. If the anchor itself is infeasible, surface that explicitly.
10. A self-optimization round is complete only when its D is verified by the configured checks."#;

const PROMPT_INTAKE: &str = "## Intake (Mode A vs Mode B)\n\
\n\
Your FIRST step in a fresh conversation is intake. Pick one of two \
modes based on the task:\n\
\n\
- **Mode A — task is clear, start the graph.** The task names a concrete \
deliverable, a specific target, or enough context to start (e.g. \
\"summarize /path/to/repo\", \"fix the bug in src/foo.rs:42\", \
\"refactor Foo::bar\"). Emit a small starting `propose_patch` (one or \
two nodes/edges) and continue.\n\
\n\
- **Mode B — task is vague, ask_user first.** The task is open to \
multiple readings, references context you don't have, or has no clear \
success criterion. Emit `ask_user` with ONE specific clarifying question \
BEFORE drawing any graph nodes. A wrong first interpretation in a \
24-round Graph phase has no recovery path — the cost of asking is much \
lower than the cost of guessing wrong.\n\
\n\
Telltale signs of Mode B (vague):\n\
- One-sentence task with no target or success criterion (\"improve the \
project\", \"what can we learn from this codebase\", \"看看这个源码\")\n\
- Task references a domain or artifact you have no internal model of\n\
- Multiple reasonable interpretations lead to different graphs\n\
- The user has not committed to a scope or deadline\n\
\n\
Telltale signs of Mode A (clear):\n\
- A specific file, function, line, or output is named\n\
- \"Add / fix / refactor / summarize / migrate X\" where X is concrete\n\
- The task continues prior context the user already established\n\
\n\
Greetings, \"what can you do\", or simple acknowledgments are NOT \
Mode B triggers — emit a small `propose_patch` (a single Task node \
capturing the conversation) and proceed.";

const PROMPT_RULES: &str = r#"## Step schemas

Always emit exactly one of these JSON objects, with no surrounding prose,
no markdown code fences, nothing else:

1. ASK USER — when the task is vague enough that drawing a graph
   now would commit you to a wrong interpretation (see the Intake
   rule below). Ask ONE specific question, not a list. Vague
   greetings ("hi", "你好", "what's up") on their own do NOT
   warrant ask_user — in those cases, propose a small initial
   patch (e.g. one Task node capturing the conversation) and let
   later rounds handle scope.
   {"step":"ask_user","question":"<one clear question>","rationale":"<why this question now>"}

2. CALL TOOL — when running a tool can answer your own question.
   Reading L2 (file contents, config, command output) almost always happens
   via tools — never invent contents.
   {"step":"call_tool","tool":"<name>","args":{...},"rationale":"<what you expect to learn>"}

3. PROPOSE PATCH — add or remove L0 nodes/edges based on what you now know.
   {"step":"propose_patch",
    "patch":{
      "add_nodes":     [{"id":"<unique>","kind":"<NodeKind>","path":"<path>","summary":"<one-line L0 hint, <=120 chars>"}],
      "add_edges":     [{"source":"<id>","target":"<id>","relation":"<RelationType>","confidence":0..1,"evidence":"<why>"}],
      "remove_node_ids":     ["<id>"],
      "remove_edge_indices": [0,3],
      "reason":"<one sentence>"
    },
    "rationale":"<why this change now>"}

   The `summary` field is a one-line L0 description — just enough for routing.
   The full L1 (responsibility/implementation/design_intent/constraints) is
   produced by the L1Enricher automatically after the patch lands; do NOT
   try to write it yourself.

4. READY FOR VERIFY — when the L0 captures the task completely.
   {"step":"ready_for_verify","rationale":"<why you believe the graph is done>"}

5. BLOCK — when you have enough context to know what to do, but
   cannot proceed without a specific human input that no tool can
   provide (a credential, a UX choice, paywalled-source output,
   a decision only the user can make). Different from `ask_user`
   which is for task clarification; this is "I'm explicitly
   blocked on something the user must give me." The run pauses
   with the reason visible to the user; they decide whether to
   unblock.
   {"step":"block",
    "reason":"<short label of what you're blocked on>",
    "needed_from_user":"<optional one-line question to surface>",
    "rationale":"<what you tried and why you're stuck>"}

6. EXPLORE — dispatch one or more subagents to do multi-file
   reads on your behalf. Use this when the next 3-5 tool calls
   would all be `cat`/`head`/`grep` against a known scope. A
   subagent with `bash` will read the files and return a summary,
   which keeps YOUR context clean and breaks the
   "I've-seen-the-directory-but-haven't-read-any-files" failure
   mode.

   You can dispatch MULTIPLE subagents in parallel by emitting
   multiple items. The subagents run concurrently; you get one
   combined summary user message when they all finish. This is
   the right move when the next questions touch DISJOINT scopes
   (e.g. "what's the orchestrator?" + "what's the web entrypoint?").
   Keep items in SEPARATE `Explore` steps when the second
   depends on the first's findings.

   Sizing rules (the runtime enforces them and will reject your
   step if you violate them — see MAX_EXPLORE_ITEMS_PER_STEP /
   MAX_EXPLORE_QUESTION_CHARS):
   - **Exactly 1 item per `Explore` step.** No parallelism across
     items. Dispatching multiple subagents in a single step
     front-loads work, makes the subagents over-stuff their
     final_answer (raw tool output + inferences), and grows the
     main conversation by 100k+ tokens per round — eventually
     blowing past the LLM's instruction-following range. Instead,
     decompose hierarchically across rounds: one round = one
     subagent = one focused scope/question. The subagent's
     summary becomes the main conversation's context for the
     next round, so each next round can dispatch a more
     informed subagent.
   - Each `question` is at most 2000 characters. If your
     question would be longer, split it into a multi-round
     sequence — first round a scan/list, next rounds
     targeted reads.

   **Post-Explore commit rule** (the runtime enforces this —
   "post-Explore commit gate" in the code): the step IMMEDIATELY
   following an `Explore` must be one of:
     - `propose_patch` — commit the subagent's findings to the
       graph (add nodes/edges describing what you learned). This
       is the normal path: every Explore should produce at least
       one graph node, otherwise you're not making progress.
     - `block` — declare you're stuck on something only the user
       can resolve.
     - `ask_user` — ask the user a clarifying question.
     - `ready_for_verify` — declare the graph is complete.
   Dispatching another `Explore` (or any other non-committing
   step) immediately after an `Explore` is a violation and the
   runtime will reject it. The flow is Explore → ProposePatch
   → Explore → ProposePatch → …, with each Explore producing
   at least one new graph node (or a corrective ProposePatch
   that fixes a previous one).

   {"step":"explore",
    "items":[
      {"scope":"<one directory / pattern / node-id list>",
       "question":"<one focused question, <=2000 chars>"}
    ],
    "rationale":"<why this subagent is the right tool here>"}

## Vocabularies (use these exact strings)

NodeKind:     File | Function | Class | Module | Config | Task | Other
RelationType: Contains | BelongsTo | Imports | Exports | DependsOn |
              Calls | Triggers | Reads | Writes | RevealedBy | InvalidatedBy | Other

(For domains that aren't code, use Other with a descriptive metadata.kind
 field — e.g. id="house:42", kind="Other", metadata={"kind":"location"}.)

## Discipline

- Output EXACTLY one JSON object. Nothing before, nothing after.
- **Pure orchestrator rule.** You have NO direct tools. If you
  emit `call_tool` it will fail with "unknown tool" — the
  only way to do anything in the world is to emit
  `explore` (dispatch a subagent that has the actual tools).
  Read this as: you are a planner, subagents are your hands.
- Be conservative. Ask the user when truly blocked — never fabricate edges.
  But respect the Intake rule (Mode A vs Mode B): if the task is vague,
  one targeted `ask_user` is much cheaper than guessing wrong. When
  the verifier or sub-agents surface a problem, fix the local node —
  don't rewrite the graph. The graph grows through many small steps.
- The `rationale` field is your **voice to the user** — not a label.
  It is rendered in the chat transcript as the assistant's message,
  so write it as a natural-language reply: if the user asked a
  question ("what model are you?", "what can you do?"), put the
  ANSWER in the rationale. If the user gave a task, briefly state
  what you're doing in plain language. Keep it to one or two
  sentences. The structured step (propose_patch / ask_user / etc.)
  is the graph side; the rationale is the human side.
- **Graph content language.** When the user's task is in a
  non-English language, write all graph content — node
  `summary`, edge `evidence`, the `reason` field on
  patches, and (if you set them) L1 fields — in **that same
  language**. The user will see the graph in the chat UI;
  English graph content next to a Chinese task is jarring and
  forces them to mentally translate. English is fine only when
  the user is also writing English.
- All edge endpoints must exist (already present, or being added in the
  same patch's add_nodes). The runtime rejects edges with missing endpoints.
- Confidence guidance: 0.9+ for evidence you directly observed (tool
  output, user statement); 0.6 for reasonable inference; <0.5 only if you
  want to flag uncertainty for verification.
- Keep each patch small. A patch that adds one or two related nodes/edges
  is normal; a patch that rebuilds half the graph is not.
- Speak the user's language when asking questions, but keep the JSON
  field names, NodeKind, and RelationType strings exactly as specified.
- L1 is not your concern in `propose_patch` — focus on L0 correctness.
  The L1 column in the graph snapshot you see each turn reflects what the
  L1Enricher has produced so far; if it looks wrong for a node, flag it as
  rationale in your next patch and the verifier/repairer will pick it up.

## drill_down (optional, in propose_patch)

Use this to mark a complex step node that needs sub-graph expansion. The
system will pause the parent graph at this node, spawn a child graph
whose `start` is this node, and the child's Filling/Expanding/Review
will produce the detail.

Schema:
  drill_down: {
    target: "<node_id from add_nodes in the same patch>",
    reason: "<one sentence: why this needs expansion>",
    sub_task_override: "<optional: refined task description for the sub-graph>"
  }

When to use:
- Node summary is broad / lists 5+ sub-items
- The node would be 1+ hour of real work
- The node has natural sub-process the user expects broken out

When NOT to use:
- Simple steps ("define the goal", "set up project")
- Atoms ("read file X", "add a label")
- Every node (max 1 drill_down per patch; sub-graph is heavy)

Example:
  propose_patch: {
    add_nodes: [{id: "design-modules", summary: "...", ...}],
    add_edges: [
      {from: "define-roles", to: "design-modules", relation: "LeadsTo"},
      {from: "design-modules", to: "define-entities", relation: "LeadsTo"}
    ],
    drill_down: {target: "design-modules", reason: "10+ sub-modules, each is a sub-design"}
  }"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FinishReason, ModelRequest, ModelResponse, Usage};
    use async_trait::async_trait;
    use std::sync::Mutex;

    // A scripted mock model — returns canned responses in order.
    struct MockModel {
        name: String,
        responses: Mutex<Vec<String>>,
        captured: Mutex<Vec<ModelRequest>>,
    }

    impl MockModel {
        fn new(responses: Vec<String>) -> Self {
            Self {
                name: "mock".into(),
                responses: Mutex::new(responses),
                captured: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Model for MockModel {
        fn name(&self) -> &str {
            &self.name
        }
        async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
            self.captured.lock().unwrap().push(req);
            let content = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| "{}".to_string());
            Ok(ModelResponse {
                content,
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                reasoning_content: None,
                usage: Usage::default(),
            })
        }
    }

    fn proposer_with(responses: Vec<&str>) -> GraphProposer {
        let model = Arc::new(MockModel::new(
            responses.iter().rev().map(|s| s.to_string()).collect(),
        ));
        let tools = Arc::new(ToolRegistry::new());
        GraphProposer::new(model, tools, None)
    }

    #[test]
    fn extract_json_unwrapped() {
        let s = r#"{"step":"ready_for_verify","rationale":"done"}"#;
        let out = extract_json_block(s).unwrap();
        assert_eq!(out, s);
    }

    #[test]
    fn extract_json_strips_markdown_fence() {
        let s = "```json\n{\"step\":\"ready_for_verify\"}\n```";
        let out = extract_json_block(s).unwrap();
        assert!(out.contains("\"step\""));
    }

    #[test]
    fn extract_json_strips_bare_fence() {
        let s = "```\n{\"step\":\"ready_for_verify\"}\n```";
        let out = extract_json_block(s).unwrap();
        assert!(out.contains("\"step\""));
    }

    #[test]
    fn extract_json_tolerates_prose_before_object() {
        let s = "Here's my step: {\"step\":\"ready_for_verify\"}";
        let out = extract_json_block(s).unwrap();
        assert!(out.starts_with('{'));
    }

    #[test]
    fn extract_json_respects_strings_with_braces() {
        // A nested brace inside a string should not throw off the balance.
        let s = r#"{"step":"ask_user","question":"is this {weird}?"}"#;
        let out = extract_json_block(s).unwrap();
        assert_eq!(out, s);
    }

    #[test]
    fn strip_think_strips_leading_block() {
        // The think block contains text that LOOKS like JSON-shaped
        // content (braces) — make sure we strip it before the
        // extractor even sees it. The helper does NOT trim
        // surrounding whitespace; `extract_json_block` does that
        // downstream.
        let s = "<think>{ some reason with } braces {{{\n</think>\n{\"step\":\"ready_for_verify\",\"rationale\":\"ok\"}";
        let out = strip_think_blocks(s);
        assert_eq!(out, "\n{\"step\":\"ready_for_verify\",\"rationale\":\"ok\"}");
    }

    #[test]
    fn strip_think_strips_multiple_blocks() {
        let s = "<think>first {reason}</think>middle<think>second {more}</think>{\"step\":\"ready_for_verify\"}";
        let out = strip_think_blocks(s);
        assert_eq!(out, "middle{\"step\":\"ready_for_verify\"}");
    }

    #[test]
    fn strip_think_handles_unterminated() {
        // No closing tag — leave the text alone rather than
        // deleting the user's actual answer.
        let s = "<think>never finished\n{\"step\":\"ready_for_verify\"}";
        let out = strip_think_blocks(s);
        assert_eq!(out, s);
    }

    #[test]
    fn strip_think_passthrough_when_absent() {
        let s = "{\"step\":\"ready_for_verify\"}";
        assert_eq!(strip_think_blocks(s), s);
    }

    #[test]
    fn extract_json_strips_think_block() {
        // End-to-end: think block + JSON should yield the same
        // result as the raw JSON.
        let raw = r#"{"step":"ready_for_verify","rationale":"ok"}"#;
        // The think block deliberately contains `}` and `{` chars
        // that a naive balance-walker would trip on.
        let wrapped = format!(
            "<think>Let me think... some text with }} and {{{{ in it\n</think>\n{raw}"
        );
        let out = extract_json_block(&wrapped).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn parse_step_ask_user() {
        let s = r#"{"step":"ask_user","question":"How many users?","rationale":"need scale"}"#;
        match parse_step(s, 1, 2000).unwrap() {
            ProposerStep::AskUser { question, options: _, rationale } => {
                assert_eq!(question, "How many users?");
                assert_eq!(rationale, "need scale");
            }
            other => panic!("expected AskUser, got {other:?}"),
        }
    }

    #[test]
    fn parse_step_call_tool() {
        let s = r#"{"step":"call_tool","tool":"bash","args":{"command":"ls"},"rationale":"see files"}"#;
        match parse_step(s, 1, 2000).unwrap() {
            ProposerStep::CallTool { tool, args, .. } => {
                assert_eq!(tool, "bash");
                assert_eq!(args.get("command").unwrap().as_str(), Some("ls"));
            }
            other => panic!("expected CallTool, got {other:?}"),
        }
    }

    #[test]
    fn parse_step_consult_advisor() {
        let s = r#"{"step":"consult_advisor","question":"which algo?","context":"sorting 1M items","rationale":"unsure"}"#;
        match parse_step(s, 1, 2000).unwrap() {
            ProposerStep::ConsultAdvisor { question, context, .. } => {
                assert_eq!(question, "which algo?");
                assert_eq!(context, "sorting 1M items");
            }
            other => panic!("expected ConsultAdvisor, got {other:?}"),
        }
    }

    #[test]
    fn parse_step_consult_advisor_from_tool_call() {
        let tc = crate::model::ToolCall {
            id: "1".into(),
            name: "consult_advisor".into(),
            arguments: serde_json::json!({"question":"q","rationale":"r"}),
        };
        match parse_step_from_tool_calls(&[tc]).unwrap() {
            ProposerStep::ConsultAdvisor { question, context, .. } => {
                assert_eq!(question, "q");
                assert!(context.is_empty());
            }
            other => panic!("expected ConsultAdvisor, got {other:?}"),
        }
    }

    #[test]
    fn propose_patch_tool_call_tolerates_missing_add_nodes() {
        // Regression: MiniMax M3 emitted a patch with only add_edges (no
        // add_nodes), which strict serde deserialization rejected with
        // "missing field `add_nodes`". The tolerant parser must accept it.
        let tc = crate::model::ToolCall {
            id: "1".into(),
            name: "propose_patch".into(),
            arguments: serde_json::json!({
                "patch": { "reason": "seed", "add_edges": [{"from": "D", "to": "A", "relation": "DependsOn"}] },
                "rationale": "r"
            }),
        };
        match parse_step_from_tool_calls(&[tc]).unwrap() {
            ProposerStep::ProposePatch { patch, .. } => {
                assert!(patch.add_nodes.is_empty());
                assert_eq!(patch.add_edges.len(), 1, "from/to aliases should map to source/target");
                assert_eq!(patch.add_edges[0].source.as_str(), "D");
                assert_eq!(patch.add_edges[0].target.as_str(), "A");
            }
            other => panic!("expected ProposePatch, got {other:?}"),
        }
    }

    #[test]
    fn propose_patch_tool_call_accepts_flat_args_without_patch_wrapper() {
        // Some models emit the patch fields at the top level instead of
        // under a "patch" key. parse_patch falls back to the whole args.
        let tc = crate::model::ToolCall {
            id: "1".into(),
            name: "propose_patch".into(),
            arguments: serde_json::json!({
                "reason": "seed",
                "add_nodes": [{"id": "A", "kind": "Task", "summary": "start"}]
            }),
        };
        match parse_step_from_tool_calls(&[tc]).unwrap() {
            ProposerStep::ProposePatch { patch, .. } => {
                assert_eq!(patch.add_nodes.len(), 1);
                assert_eq!(patch.add_nodes[0].id.as_str(), "A");
            }
            other => panic!("expected ProposePatch, got {other:?}"),
        }
    }

    #[test]
    fn proposer_tools_includes_advisor_only_when_enabled() {
        let without = proposer_tools(false);
        assert!(!without.iter().any(|t| t["function"]["name"] == "consult_advisor"));
        let with = proposer_tools(true);
        assert!(with.iter().any(|t| t["function"]["name"] == "consult_advisor"));
    }

    #[test]
    fn parse_step_propose_patch() {
        let s = r#"{
          "step":"propose_patch",
          "patch":{
            "add_nodes":[{"id":"u","kind":"Other","path":"u","summary":"user node"}],
            "add_edges":[{"source":"u","target":"v","relation":"DependsOn","confidence":0.8,"evidence":"stated"}],
            "remove_edge_indices":[2],
            "reason":"capture dep"
          },
          "rationale":"new info from user"
        }"#;
        match parse_step(s, 1, 2000).unwrap() {
            ProposerStep::ProposePatch { patch, .. } => {
                assert_eq!(patch.add_nodes.len(), 1);
                assert_eq!(patch.add_nodes[0].id.as_str(), "u");
                assert_eq!(patch.add_edges.len(), 1);
                assert!(matches!(patch.add_edges[0].relation, RelationType::DependsOn));
                assert!((patch.add_edges[0].confidence - 0.8).abs() < 1e-9);
                assert_eq!(patch.remove_edge_indices, vec![2]);
                assert_eq!(patch.reason, "capture dep");
            }
            other => panic!("expected ProposePatch, got {other:?}"),
        }
    }

    #[test]
    fn block_step_kind_is_block() {
        // Regression guard: the system prompt teaches the model to
        // emit step="block" with reason/needed_from_user/rationale.
        // If a future refactor renames the variant, this test names
        // exactly what changed.
        let s = ProposerStep::Block {
            reason: "need API key".into(),
            needed_from_user: "Do you have a Stripe test key?".into(),
            rationale: "I'm ready to call but auth blocks me".into(),
        };
        assert_eq!(s.kind(), "block");
    }

    #[test]
    fn block_step_round_trips_through_parse_step() {
        // A model that emits the documented `block` JSON should
        // parse back to the Block variant. If the JSON shape
        // changes, this fails.
        let s = parse_step(
            r#"{"step":"block","reason":"need credential","needed_from_user":"which key?","rationale":"I tried"}"#,
            1,
            2000,
        )
        .unwrap();
        match s {
            ProposerStep::Block { reason, needed_from_user, rationale } => {
                assert_eq!(reason, "need credential");
                assert_eq!(needed_from_user, "which key?");
                assert_eq!(rationale, "I tried");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn explore_step_kind_is_explore() {
        // Regression guard: the system prompt teaches the model
        // to emit step="explore" with items[].scope/question
        // and rationale.
        let s = ProposerStep::Explore {
            items: vec![ExploreItem {
                scope: "src/agent/".into(),
                question: "what's the orchestrator pattern?".into(),
            }],
            rationale: "I've seen the directory but not read any files".into(),
        };
        assert_eq!(s.kind(), "explore");
    }

    #[test]
    fn explore_step_round_trips_through_parse_step() {
        // A model that emits the documented `explore` JSON with
        // `items` (the new multi-item shape) should parse back
        // to the Explore variant. If the JSON shape changes,
        // this fails.
        let s = parse_step(
            r#"{"step":"explore","items":[{"scope":"src/agent/","question":"what's the orchestrator pattern?"}],"rationale":"seen dir, not read files"}"#,
            1,
            2000,
        )
        .unwrap();
        match s {
            ProposerStep::Explore { items, rationale } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].scope, "src/agent/");
                assert_eq!(items[0].question, "what's the orchestrator pattern?");
                assert_eq!(rationale, "seen dir, not read files");
            }
            other => panic!("expected Explore, got {other:?}"),
        }
    }

    #[test]
    fn explore_step_round_trips_through_parse_step_legacy_shape() {
        // Backwards compat: a model that emits the old
        // flat `scope`+`question` (pre-multi-item) should also
        // parse, with a single-item vec.
        let s = parse_step(
            r#"{"step":"explore","scope":"src/agent/","question":"what's the orchestrator pattern?","rationale":"r"}"#,
            1,
            2000,
        )
        .unwrap();
        match s {
            ProposerStep::Explore { items, .. } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].scope, "src/agent/");
                assert_eq!(items[0].question, "what's the orchestrator pattern?");
            }
            other => panic!("expected Explore, got {other:?}"),
        }
    }

    #[test]
    fn explore_step_rejects_multiple_items_per_cap() {
        // Cap is 1 item per Explore step (post-2026-06-09). The model
        // is supposed to dispatch one subagent per step and decompose
        // hierarchically across rounds; multi-item parallel dispatch
        // front-loads work and produces 800k-token final_answers.
        // The parse path now rejects this with a clear error.
        let r = parse_step(
            r#"{"step":"explore","items":[
                {"scope":"src/agent/","question":"what's the orchestrator?"},
                {"scope":"src/web/","question":"what's the web entrypoint?"}
            ],"rationale":"two scopes"}"#,
            1,
            2000,
        );
        let err = r.unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("cap is 1"), "expected cap error, got: {msg}");
        assert!(msg.contains("Split into two"), "expected split hint, got: {msg}");
    }

    #[test]
    fn explore_step_rejects_empty_items_or_empty_item_fields() {
        // No items at all → fail (the dispatcher would no-op).
        assert!(parse_step(
            r#"{"step":"explore","items":[],"rationale":"r"}"#,
            1,
            2000,
        )
        .is_err());
        // An item with empty scope → fail.
        assert!(parse_step(
            r#"{"step":"explore","items":[{"scope":"","question":"x"}],"rationale":"r"}"#,
            1,
            2000,
        )
        .is_err());
        // An item with empty question → fail.
        assert!(parse_step(
            r#"{"step":"explore","items":[{"scope":"x","question":""}],"rationale":"r"}"#,
            1,
            2000,
        )
        .is_err());
    }

    #[test]
    fn parse_step_unknown_kind_falls_to_other() {
        let s = r#"{
          "step":"propose_patch",
          "patch":{
            "add_nodes":[{"id":"x","kind":"BoardMeeting","path":"x","summary":""}],
            "add_edges":[],
            "reason":""
          }
        }"#;
        match parse_step(s, 1, 2000).unwrap() {
            ProposerStep::ProposePatch { patch, .. } => match &patch.add_nodes[0].kind {
                NodeKind::Other(name) => assert_eq!(name, "BoardMeeting"),
                other => panic!("expected NodeKind::Other, got {other:?}"),
            },
            other => panic!("expected ProposePatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_step_unknown_relation_falls_to_other() {
        let s = r#"{
          "step":"propose_patch",
          "patch":{
            "add_nodes":[{"id":"a","kind":"File","path":"a","summary":""},
                          {"id":"b","kind":"File","path":"b","summary":""}],
            "add_edges":[{"source":"a","target":"b","relation":"SoftCoupling","confidence":0.5,"evidence":""}]
          }
        }"#;
        match parse_step(s, 1, 2000).unwrap() {
            ProposerStep::ProposePatch { patch, .. } => {
                match &patch.add_edges[0].relation {
                    RelationType::Other(s) => assert_eq!(s, "SoftCoupling"),
                    other => panic!("expected RelationType::Other, got {other:?}"),
                }
            }
            _ => panic!("expected ProposePatch"),
        }
    }

    #[test]
    fn parse_step_leads_to_relation_is_structural() {
        // Regression (run 1a55f6a1, GATE-DIAG 2026-06-24): parse_relation_type
        // was missing the "LeadsTo" arm, so every model-proposed chain edge
        // became Other("LeadsTo"), whose is_structural() is false. path_exists
        // then ignored the entire chain → start could not reach any middle
        // node → verify gate falsely reported all-orphans → infinite thrash.
        // LeadsTo is THE flow edge; it must parse to the canonical variant.
        let s = r#"{
          "step":"propose_patch",
          "patch":{
            "add_nodes":[{"id":"start","kind":"Task","path":"start","summary":""},
                          {"id":"mid","kind":"Task","path":"mid","summary":""}],
            "add_edges":[{"source":"start","target":"mid","relation":"LeadsTo","confidence":0.9,"evidence":""}]
          }
        }"#;
        match parse_step(s, 1, 2000).unwrap() {
            ProposerStep::ProposePatch { patch, .. } => {
                assert_eq!(
                    patch.add_edges[0].relation,
                    RelationType::LeadsTo,
                    "LeadsTo must parse to the canonical variant, not Other"
                );
                assert!(
                    patch.add_edges[0].relation.is_structural(),
                    "LeadsTo must be structural so path_exists walks it"
                );
            }
            _ => panic!("expected ProposePatch"),
        }
    }

    #[test]
    fn parse_step_ready_for_verify_minimal() {
        let s = r#"{"step":"ready_for_verify"}"#;
        match parse_step(s, 1, 2000).unwrap() {
            ProposerStep::ReadyForVerify { rationale } => assert!(rationale.is_empty()),
            other => panic!("expected ReadyForVerify, got {other:?}"),
        }
    }

    #[test]
    fn parse_step_missing_step_field_errors() {
        let s = r#"{"foo":"bar"}"#;
        let err = parse_step(s, 1, 2000).unwrap_err();
        assert!(format!("{err}").contains("missing 'step'"));
    }

    #[test]
    fn parse_step_unknown_step_errors() {
        let s = r#"{"step":"refactor_universe"}"#;
        let err = parse_step(s, 1, 2000).unwrap_err();
        assert!(format!("{err}").contains("unknown step"));
    }

    #[test]
    fn parse_step_malformed_json_errors() {
        let s = "not even JSON here";
        let err = parse_step(s, 1, 2000).unwrap_err();
        // Either no `{` or invalid JSON — both are acceptable here.
        assert!(format!("{err}").to_lowercase().contains("json")
            || format!("{err}").contains("'{'"));
    }

    // ---- extract_json_block robustness ----

    #[test]
    fn extract_json_handles_leading_prose() {
        // Model emits natural-language thinking before the JSON.
        // The outermost `{` in the prose (if any) or the actual JSON should work.
        let text = "I'll help with that.\n\n{\"step\":\"ready_for_verify\",\"rationale\":\"done\"}";
        let block = extract_json_block(text).unwrap();
        let v: serde_json::Value = serde_json::from_str(&block).unwrap();
        assert_eq!(v["step"], "ready_for_verify");
    }

    #[test]
    fn extract_json_handles_think_and_json() {
        // `<think>` block with reasoning, then clean JSON.
        let text = "<think>Need to add a node for the new module</think>\n{\"step\":\"propose_patch\",\"patch\":{\"add_nodes\":[],\"add_edges\":[],\"remove_node_ids\":[],\"remove_edge_indices\":[],\"set_l1\":{}},\"rationale\":\"adding\"}";
        let block = extract_json_block(text).unwrap();
        let v: serde_json::Value = serde_json::from_str(&block).unwrap();
        assert_eq!(v["step"], "propose_patch");
    }

    #[test]
    fn extract_json_handles_prose_with_braces_in_example() {
        // Prose that contains `{` (e.g. JSON example in thinking), then the real JSON.
        let text = "Here's an example: {\"foo\": \"bar\"} — now my real answer:\n{\"step\":\"ready_for_verify\",\"rationale\":\"ok\"}";
        let block = extract_json_block(text).unwrap();
        let v: serde_json::Value = serde_json::from_str(&block).unwrap();
        assert_eq!(v["step"], "ready_for_verify");
    }

    #[test]
    fn extract_json_handles_code_fence_with_prose() {
        // ```json fence with leading prose after the fence.
        let text = "```json\n{\"step\":\"ready_for_verify\",\"rationale\":\"done\"}\n```";
        let block = extract_json_block(text).unwrap();
        let v: serde_json::Value = serde_json::from_str(&block).unwrap();
        assert_eq!(v["step"], "ready_for_verify");
    }

    #[test]
    fn extract_json_picks_last_valid_when_multiple_outermost() {
        // Two outermost JSON blocks — the last one should win.
        let text = "{\"a\":1}\nsome text\n{\"step\":\"ready_for_verify\",\"rationale\":\"last\"}";
        let block = extract_json_block(text).unwrap();
        let v: serde_json::Value = serde_json::from_str(&block).unwrap();
        assert_eq!(v["step"], "ready_for_verify");
    }

    #[test]
    fn extract_json_no_braces_at_all() {
        let text = "just plain text, no JSON anywhere";
        let err = extract_json_block(text).unwrap_err();
        assert!(format!("{err}").contains("'{'"));
    }

    #[tokio::test]
    async fn next_step_calls_model_and_parses() {
        let p = proposer_with(vec![r#"{"step":"ready_for_verify","rationale":"trivial"}"#]);
        let conv = p.make_conversation("test task");
        let graph = Graph::new();
        let (step, tokens) = p.next_step(&conv, &graph, None).await.unwrap();
        match step {
            ProposerStep::ReadyForVerify { rationale } => assert_eq!(rationale, "trivial"),
            other => panic!("expected ReadyForVerify, got {other:?}"),
        }
        // Mock model returns Usage::default() → total_tokens = 0
        assert_eq!(tokens, 0);
    }

    #[tokio::test]
    async fn next_step_with_retry_succeeds_when_second_attempt_is_valid() {
        // First response is plain text with no JSON; second is valid.
        // The retry should call the model twice and return the second
        // step.
        let p = proposer_with(vec![
            r#"{"step":"ask_user","question":"what now?","rationale":"after retry"}"#,
            "I don't know what to put in JSON, sorry",
        ]);
        let conv = p.make_conversation("test task");
        let graph = Graph::new();
        let (step, _tokens) = p.next_step_with_retry(&conv, &graph, None).await.unwrap();
        match step {
            ProposerStep::AskUser { question, options: _, rationale } => {
                assert_eq!(question, "what now?");
                assert_eq!(rationale, "after retry");
            }
            other => panic!("expected AskUser, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn next_step_rejects_explore_after_explore_without_commit() {
        // Post-Explore commit gate (added 2026-06-09): after an Explore
        // step, the next step must be a graph-committing or
        // pause-declaring step. A second Explore is rejected.
        use crate::agent::proposer::ExploreItem;
        let prev = ProposerStep::Explore {
            items: vec![ExploreItem {
                scope: "src/".into(),
                question: "what's the structure?".into(),
            }],
            rationale: "r".into(),
        };
        // Model tries to dispatch another Explore right after the
        // first one — should be rejected.
        let p = proposer_with(vec![
            r#"{"step":"explore","items":[{"scope":"src/web/","question":"what's the entry?"}],"rationale":"more"}"#,
        ]);
        let conv = p.make_conversation("test task");
        let graph = Graph::new();
        let r = p.next_step(&conv, &graph, Some(&prev)).await;
        let err = r.unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("post-Explore commit gate"),
            "expected post-Explore commit gate error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn next_step_accepts_propose_patch_after_explore() {
        // The normal path: Explore → ProposePatch (commit findings).
        // Should pass without error.
        use crate::agent::proposer::ExploreItem;
        let prev = ProposerStep::Explore {
            items: vec![ExploreItem {
                scope: "src/".into(),
                question: "what's the structure?".into(),
            }],
            rationale: "r".into(),
        };
        let p = proposer_with(vec![r#"{"step":"propose_patch","patch":{"add_nodes":[],"add_edges":[],"remove_node_ids":[],"remove_edge_indices":[],"reason":"committed"},"rationale":"committing"}"#]);
        let conv = p.make_conversation("test task");
        let graph = Graph::new();
        let (step, _tokens) = p
            .next_step(&conv, &graph, Some(&prev))
            .await
            .unwrap();
        assert!(matches!(step, ProposerStep::ProposePatch { .. }));
    }

    #[tokio::test]
    async fn next_step_accepts_explore_when_no_prev_step() {
        // First round of a fresh conversation has no prev_step, so
        // Explore is always allowed.
        let p = proposer_with(vec![r#"{"step":"explore","items":[{"scope":"src/","question":"what?"}],"rationale":"r"}"#]);
        let conv = p.make_conversation("test task");
        let graph = Graph::new();
        let (step, _tokens) = p.next_step(&conv, &graph, None).await.unwrap();
        assert!(matches!(step, ProposerStep::Explore { .. }));
    }

    #[tokio::test]
    async fn next_step_with_retry_salvages_ask_user_when_both_attempts_fail() {
        // When both attempts fail, the function salvages an AskUser step
        // instead of dying — keeps the loop alive for heartbeat/auto-answer.
        let p = proposer_with(vec![
            "still bad on second try",
            "still bad on second try",
        ]);
        let conv = p.make_conversation("test task");
        let graph = Graph::new();
        let (step, _tokens) = p
            .next_step_with_retry(&conv, &graph, None)
            .await
            .expect("salvage should produce a step");
        match step {
            ProposerStep::AskUser { question, options: _, rationale } => {
                assert!(
                    rationale.to_lowercase().contains("falling") || rationale.contains("salvage"),
                    "rationale should mention salvage/fallback: {rationale}"
                );
                assert!(!question.is_empty());
            }
            other => panic!("expected AskUser salvage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn next_step_request_includes_graph_snapshot() {
        let p = proposer_with(vec![r#"{"step":"ready_for_verify"}"#]);
        let conv = p.make_conversation("test");
        let mut g = Graph::new();
        g.add_node(Node::file("hello.rs", "greeting"));
        let _ = p.next_step(&conv, &g, None).await.unwrap();
        // Check that the captured request's system message contains the node
        let model = p.model.clone();
        // Downcast trick: we only have Arc<dyn Model>; use the inherent test API.
        // Inspect via the request the mock captured.
        let mock = model
            .as_any_mock();
        let captured = mock.captured.lock().unwrap();
        let req = captured.last().unwrap();
        let snapshot_msg = req
            .messages
            .iter()
            .find(|m| matches!(m.role, Role::System) && m.content.contains("graph version="))
            .expect("graph snapshot system message present");
        assert!(snapshot_msg.content.contains("hello.rs"));
    }

    #[tokio::test]
    async fn system_prompt_mentions_three_layer_graph() {
        // Prompt should educate the model about L0/L1/L2 distinction.
        let p = proposer_with(vec![r#"{"step":"ready_for_verify"}"#]);
        let prompt = p.build_system_prompt("any task");
        assert!(prompt.contains("L0"), "missing L0 mention");
        assert!(prompt.contains("L1"), "missing L1 mention");
        assert!(prompt.contains("L2"), "missing L2 mention");
        // Should tell model NOT to write L1 (enricher's job)
        assert!(
            prompt.contains("L1Enricher") || prompt.contains("automatically"),
            "missing instruction about L1 enrichment"
        );
    }

    #[test]
    fn proposer_system_prompt_codifies_mode_b_intake() {
        // Regression guard for the "Core idea" / Mode B rule (ARCHITECTURE
        // §1a). The system prompt must explicitly teach the model to
        // recognize vague tasks and prefer `ask_user` BEFORE drawing any
        // graph nodes. If a future refactor drops this rule, the model
        // will fall back to guessing — and a wrong first interpretation
        // has no recovery path inside a 24-round Graph phase.
        let p = proposer_with(vec![r#"{"step":"ready_for_verify"}"#]);
        let prompt = p.build_system_prompt("any task");

        // The Intake section must exist and be named.
        assert!(
            prompt.contains("Intake") || prompt.contains("intake"),
            "system prompt missing the Intake section header"
        );
        // Both modes must be named, with a clear signal that Mode B
        // is the one to pick for vague tasks.
        assert!(prompt.contains("Mode A"), "system prompt missing Mode A label");
        assert!(prompt.contains("Mode B"), "system prompt missing Mode B label");
        // The "vague task → ask_user FIRST" connection has to be there.
        assert!(
            prompt.contains("ask_user") && prompt.contains("BEFORE"),
            "system prompt must teach the model to ask_user BEFORE drawing nodes for vague tasks"
        );
        // The "vague task has no recovery path" framing is the key
        // motivation — without it the rule reads as a soft preference.
        assert!(
            prompt.contains("24-round") || prompt.contains("no recovery"),
            "system prompt must include the 'no recovery' justification for Mode B"
        );
        // Anti-pattern guard: the old "default toward action; ask_user
        // is stalling" framing contradicts Mode B. It must be gone.
        assert!(
            !prompt.contains("stalling with a clarifying question"),
            "system prompt still contains the old 'ask_user is stalling' anti-pattern; Mode B is contradicted"
        );
        assert!(
            !prompt.contains("one tiny ask_user is fine, repeated ask_user is not"),
            "system prompt still contains the old 'one ask_user is fine' rule that conflicts with Mode B"
        );
    }

    #[test]
    fn proposer_system_prompt_includes_skills_section_when_storage_set() {
        use crate::graph::Graph;
        use crate::skills::storage::{LocalSkillStorage, SkillStorage};
        use crate::skills::types::{Skill, SkillMeta};

        let dir = tempfile::TempDir::new().unwrap();
        let storage = LocalSkillStorage::new(dir.path().to_path_buf());
        let skill = Skill {
            slug: "demo-skill".to_string(),
            task: "do X".to_string(),
            trigger: "applies when X is needed".to_string(),
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
        storage.save(&skill).unwrap();

        let storage_arc: Arc<dyn crate::skills::SkillStorage> = Arc::new(storage);
        let model = Arc::new(MockModel::new(vec![]));
        let proposer = GraphProposer::new(
            model,
            Arc::new(ToolRegistry::new()),
            Some(storage_arc),
        );
        let prompt = proposer.build_system_prompt("any task");
        assert!(prompt.contains("## Available skills"));
        assert!(prompt.contains("demo-skill"));
    }

    #[test]
    fn proposer_system_prompt_omits_skills_section_when_storage_none() {
        let model = Arc::new(MockModel::new(vec![]));
        let proposer = GraphProposer::new(
            model,
            Arc::new(ToolRegistry::new()),
            None,
        );
        let prompt = proposer.build_system_prompt("any task");
        assert!(!prompt.contains("## Available skills"));
    }

    #[test]
    fn proposer_system_prompt_contains_drill_down_schema() {
        // The system prompt must document the `drill_down` field on
        // `propose_patch` so the model knows it can mark a complex step
        // node for sub-graph expansion. Without this schema, the model
        // never emits `drill_down` and Task 6 (fork_sub_graph_for) is
        // never exercised.
        let p = proposer_with(vec![r#"{"step":"ready_for_verify"}"#]);
        let prompt = p.build_system_prompt("any task");
        assert!(prompt.contains("drill_down"), "prompt missing 'drill_down' keyword");
        assert!(prompt.contains("target"), "prompt missing 'target' field doc");
        assert!(prompt.contains("sub_task_override"), "prompt missing 'sub_task_override' field doc");
        assert!(prompt.contains("design-modules"), "prompt missing example node id");
    }

    #[tokio::test]
    async fn graph_snapshot_includes_l1_state_next_to_each_node() {
        // When L1 exists, snapshot shows it. When L1 missing, shows "not yet enriched".
        let p = proposer_with(vec![r#"{"step":"ready_for_verify"}"#]);
        let conv = p.make_conversation("test");
        let mut g = Graph::new();
        g.add_node(Node::file("with_l1.rs", "X"));
        g.add_node(Node::file("no_l1.rs", "Y"));
        g.l1.set(
            NodeId::from("with_l1.rs"),
            crate::graph::L1Description::new("does X", "wraps Y", "for Z", "always W")
                .with_confidence(0.85),
        );
        let _ = p.next_step(&conv, &g, None).await.unwrap();
        let model_arc = p.model.clone();
        let mock = model_arc.as_any_mock();
        let captured = mock.captured.lock().unwrap();
        let snapshot = captured
            .last()
            .unwrap()
            .messages
            .iter()
            .find(|m| matches!(m.role, Role::System) && m.content.contains("graph version="))
            .unwrap();
        assert!(snapshot.content.contains("does X"), "with_l1.rs should show L1");
        assert!(
            snapshot.content.contains("not yet enriched"),
            "no_l1.rs should mark itself as un-enriched"
        );
    }

    // Helper: downcast Arc<dyn Model> back to &MockModel for inspection.
    // We do this only inside tests; production code never needs it.
    trait MockProbe {
        fn as_any_mock(&self) -> &MockModel;
    }
    impl MockProbe for Arc<dyn Model> {
        fn as_any_mock(&self) -> &MockModel {
            // This is sound only because we know in the tests above that the
            // Arc holds a MockModel. We use raw-pointer reinterpretation
            // because dyn Model isn't downcast-able by default.
            let raw = Arc::as_ptr(self) as *const MockModel;
            unsafe { &*raw }
        }
    }

    // ----- drill_down validation tests (Task 2) -----
    //
    // `validate_drill_down` is the unit-level gate. The e2e tests below
    // exercise the full pipeline: raw JSON → parse_patch → drill_down
    // populated → validate_drill_down result. Without these, a regression
    // in `parse_patch` (e.g. dropping the new drill_down extraction code)
    // would silently make the validator dead code again.

    #[test]
    fn validate_drill_down_returns_err_on_missing_target() {
        let patch = GraphPatch {
            add_nodes: vec![Node::task("design-modules", "...")],
            add_edges: vec![],
            remove_node_ids: vec![],
            remove_edge_indices: vec![],
            set_l1: vec![],
            reason: "test".into(),
            drill_down: Some(DrillDownMark {
                target: NodeId::from("not-in-add-nodes"),
                reason: "test".into(),
                sub_task_override: None,
            }),
        };
        assert!(validate_drill_down(&patch).is_err());
    }

    #[test]
    fn validate_drill_down_returns_ok_on_valid_target() {
        let patch = GraphPatch {
            add_nodes: vec![Node::task("design-modules", "...")],
            add_edges: vec![],
            remove_node_ids: vec![],
            remove_edge_indices: vec![],
            set_l1: vec![],
            reason: "test".into(),
            drill_down: Some(DrillDownMark {
                target: NodeId::from("design-modules"),
                reason: "test".into(),
                sub_task_override: None,
            }),
        };
        assert!(validate_drill_down(&patch).is_ok());
    }

    #[test]
    fn validate_drill_down_returns_ok_when_field_absent() {
        let patch = GraphPatch::default();
        assert!(validate_drill_down(&patch).is_ok());
    }

    // ----- e2e: raw JSON → parse_patch → drill_down populated -----
    //
    // These exercise the same path `parse_step` / `parse_step_from_tool_calls`
    // walk in production: raw JSON string parsed into a serde_json::Value,
    // then handed to `parse_patch`. They catch regressions where
    // `parse_patch` stops reading the `drill_down` field (which would
    // silently regress the validator from commit 145e8f7 to dead code).

    #[test]
    fn parse_patch_extracts_drill_down_from_json() {
        let raw = r#"{
            "reason": "x",
            "add_nodes": [
                {"id": "design-modules", "kind": "Task", "path": "design-modules", "summary": "design"}
            ],
            "drill_down": {
                "target": "design-modules",
                "reason": "expand the module design"
            }
        }"#;
        let v: serde_json::Value = serde_json::from_str(raw).expect("raw parses");
        let patch = parse_patch(&v).expect("parse_patch succeeds");
        assert!(
            patch.drill_down.is_some(),
            "parse_patch must extract drill_down from JSON; got None"
        );
        let dd = patch.drill_down.as_ref().unwrap();
        assert_eq!(dd.target.as_str(), "design-modules");
        assert_eq!(dd.reason, "expand the module design");
        assert!(dd.sub_task_override.is_none());
    }

    #[test]
    fn parse_patch_omits_drill_down_when_field_absent() {
        let raw = r#"{
            "reason": "x",
            "add_nodes": [
                {"id": "design-modules", "kind": "Task", "path": "design-modules", "summary": "design"}
            ]
        }"#;
        let v: serde_json::Value = serde_json::from_str(raw).expect("raw parses");
        let patch = parse_patch(&v).expect("parse_patch succeeds");
        assert!(
            patch.drill_down.is_none(),
            "drill_down must default to None when absent"
        );
    }

    #[test]
    fn parse_patch_tolerates_malformed_drill_down() {
        // target is missing → deserialize fails → patch.drill_down = None,
        // but the rest of the patch still parses (no Err returned).
        let raw = r#"{
            "reason": "x",
            "add_nodes": [
                {"id": "design-modules", "kind": "Task", "path": "design-modules", "summary": "design"}
            ],
            "drill_down": {"reason": "missing target field"}
        }"#;
        let v: serde_json::Value = serde_json::from_str(raw).expect("raw parses");
        let patch = parse_patch(&v).expect("parse_patch should tolerate malformed drill_down");
        assert!(
            patch.drill_down.is_none(),
            "malformed drill_down must not populate the field"
        );
        assert_eq!(patch.add_nodes.len(), 1, "rest of patch must survive");
    }
}
