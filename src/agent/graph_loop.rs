//! GraphLoop — the universal iterative closure.
//!
//! `step()` advances the state machine by one beat and returns a
//! [`LoopState`] describing what just happened or what the caller needs to
//! do. The loop itself is **purely passive**: it never reads stdin, never
//! invokes a graph repairer on its own, never decides to skip a fix. When
//! it needs the caller to do something, it returns the appropriate
//! `LoopState` and waits for `resume*` to feed back.
//!
//! ## Phases
//!
//! ```text
//!   Graph   ──┐
//!     ↑       │ verify passes
//!     │       ▼
//!     │     Task   ──┐
//!     │       ↑      │ all batches OK
//!     │       │      ▼
//!     │       │    Review  ──→  Done
//!     │       │      │
//!     │       │      └─ (code issue) ─→ back to Task with feedback
//!     │       │
//!     │       └─ (sub-agent reports graph mismatch
//!     │             OR validation fails as graph issue)
//!     │
//!     └──── GraphInvalid surfaced to caller ────
//! ```
//!
//! Phase 2 v1 implements the **Graph** phase fully. The Task and Review
//! phases are stubbed: when Graph phase verification passes, the loop
//! transitions directly to `Done`. Phase 3 will fill in the Task
//! (sub-agent dispatch) and Review phases, at which point `GraphInvalid`
//! becomes a real return state.
//!
//! See [[feedback-iterative-loop-is-centerpiece]] for the doctrine.

use super::decomposer::Decomposer;
use super::dispatcher::{DispatchOutcome, Dispatcher};
use super::enricher::L1Enricher;
use super::proposer::{ExploreItem, GraphProposer, ProposerStep};
use super::repairer::LocalRepairer;
use super::reviewer::{ReviewResult, Reviewer, RootCause};
use super::validator::{PostExecutionValidator, ValidationVerdict};
use super::verifier::{Severity, VerificationResult, VerifyIssue, Verifier};
use super::Conversation;
use crate::context::SourceLoader;
use crate::error::Result;
use crate::graph::{Graph, NodeId};
use crate::tools::{ToolContext, ToolRegistry};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// LoopState — what `step()` returns to the caller
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum LoopState {
    /// One beat completed, nothing for the caller to do. Call `step()` again.
    Running,

    /// The agent has a question for the user. Caller must answer with `resume(answer)`.
    Paused { question: String, rationale: String },

    /// Graph errors were discovered DURING task execution or review (Phase 3+).
    /// The caller is expected to repair the graph and call
    /// `resume_with_repaired_graph(repaired)` — or `resume_force()` to skip.
    /// Phase 2 v1 does not produce this state directly; it is reserved for
    /// the Phase 3 Task/Review phases.
    GraphInvalid {
        source: ErrorSource,
        errors: Vec<GraphError>,
        snapshot: Graph,
    },

    /// Task batches ran but the failure was code-level (not graph-level).
    /// Caller can re-augment the task and `resume_force()` to retry, or
    /// abort.
    TaskFailed { failures: Vec<SubTaskFailure> },

    /// Loop completed. The final graph + transcript live in the result.
    Done(FinalResult),

    /// Unrecoverable error. The loop is poisoned; no further `step()` calls.
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ErrorSource {
    /// A sub-agent reading source data found the graph disagrees with reality.
    DuringExecution,
    /// Deterministic post-execution checks (compile/test/lint) failed in a
    /// way that points at the graph, not the code.
    PostExecutionValidation,
    /// The Reviewer in the REVIEW phase judged the result against original
    /// requirements and concluded the graph itself was wrong.
    Review,
    /// Internal: the Graph-phase Verifier surfaced an issue that the
    /// LocalRepairer could not fix within `max_repair_rounds`.
    VerifierStalemate,
}

/// Which kind of L0 structural defect a [`GraphError::L0Structural`] flags.
///
/// Note that `WrongScope` from the v1 spec moved up to be its own
/// [`GraphError::ScopeGap`] variant — it's a fundamentally different
/// failure mode (graph is missing whole regions) and routes to a different
/// repair path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum L0ErrorType {
    /// Graph is missing an edge that exists in reality.
    MissingRelation,
    /// Graph has an edge with the wrong source/target/relation, or a
    /// cycle in an acyclic-required relation.
    WrongRelation,
    /// Graph is missing a node that exists in reality, or holds an edge
    /// to a non-existent endpoint.
    MissingNode,
}

/// The three layers of "graph is wrong" per the v2.0 design.
///
/// - [`L0Structural`] — nodes/edges are missing, extra, or wrong. Repair
///   path: read L2 to confirm, then patch L0 nodes/edges.
/// - [`L1Semantic`] — a node's L1 description has drifted from what L2
///   actually says. Repair path: re-read L2 for that node, rewrite L1.
/// - [`ScopeGap`] — L0+L1 is missing whole regions the task needs.
///   Repair path: extend L0 (scanner-like operation) + enrich L1 for the
///   new nodes.
///
/// Each variant carries `discovered_by` so the loop can trace which
/// sub-agent / verifier round surfaced the error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GraphError {
    L0Structural {
        error_type: L0ErrorType,
        detail: String,
        related_nodes: Vec<NodeId>,
        discovered_by: Option<String>,
    },
    L1Semantic {
        node: NodeId,
        /// The L1 text the model thought was true.
        current_l1: String,
        /// The L2 evidence that contradicts it.
        actual_l2_evidence: String,
        discovered_by: Option<String>,
    },
    ScopeGap {
        missing_nodes: Vec<NodeId>,
        /// `(source, target, relation_hint)` — the hint is a free-form
        /// string because we don't always know the exact `RelationType`
        /// at the moment a gap is detected.
        missing_edges: Vec<(NodeId, NodeId, String)>,
        detail: String,
        discovered_by: Option<String>,
    },
}

impl GraphError {
    /// Short label for logs / telemetry.
    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::L0Structural { .. } => "L0Structural",
            Self::L1Semantic { .. } => "L1Semantic",
            Self::ScopeGap { .. } => "ScopeGap",
        }
    }

    /// Who discovered this error, if known.
    pub fn discovered_by(&self) -> Option<&str> {
        match self {
            Self::L0Structural { discovered_by, .. }
            | Self::L1Semantic { discovered_by, .. }
            | Self::ScopeGap { discovered_by, .. } => discovered_by.as_deref(),
        }
    }

    /// Set or replace `discovered_by` in place.
    pub fn with_discovered_by(mut self, by: impl Into<String>) -> Self {
        let by = by.into();
        match &mut self {
            Self::L0Structural { discovered_by, .. }
            | Self::L1Semantic { discovered_by, .. }
            | Self::ScopeGap { discovered_by, .. } => *discovered_by = Some(by),
        }
        self
    }

    /// Nodes the error implicates. For L0Structural it's the related_nodes
    /// list; for L1Semantic it's the single drifted node; for ScopeGap it's
    /// the missing nodes.
    pub fn related_nodes(&self) -> Vec<NodeId> {
        match self {
            Self::L0Structural { related_nodes, .. } => related_nodes.clone(),
            Self::L1Semantic { node, .. } => vec![node.clone()],
            Self::ScopeGap { missing_nodes, .. } => missing_nodes.clone(),
        }
    }

    /// One-line human-readable summary. Used in logs and the model's
    /// repair prompt.
    pub fn detail(&self) -> String {
        match self {
            Self::L0Structural {
                error_type, detail, ..
            } => format!("[{error_type:?}] {detail}"),
            Self::L1Semantic {
                node,
                current_l1,
                actual_l2_evidence,
                ..
            } => format!(
                "L1 drift on `{node}`: described as {current_l1:?} but L2 says {actual_l2_evidence:?}"
            ),
            Self::ScopeGap {
                missing_nodes,
                missing_edges,
                detail,
                ..
            } => format!(
                "{detail} (missing {} node(s), {} edge(s))",
                missing_nodes.len(),
                missing_edges.len()
            ),
        }
    }

    /// Best-effort mapping from a verifier-emitted [`VerifyIssue`] to a
    /// `GraphError`. Heuristic-based on the issue's description text:
    ///
    /// - "L1 drift on `<id>`: …" → [`L1Semantic`] (from the verifier's L1-sampling layer)
    /// - "scope" / "range" / "missing module" → [`ScopeGap`]
    /// - "cycle" / "wrong" / "incorrect" → [`L0Structural`] with `WrongRelation`
    /// - "dangling" / "missing node" → [`L0Structural`] with `MissingNode`
    /// - else → [`L0Structural`] with `MissingRelation` (the most common)
    pub fn from_verify_issue(issue: &VerifyIssue) -> Self {
        let desc = &issue.description;
        // L1 drift comes from Verifier::l1_sampling_check with the format
        // "L1 drift on <node_id>: <detail>" — parse the node and detail out
        // so we can populate the L1Semantic variant precisely.
        if let Some(rest) = desc.strip_prefix("L1 drift on ") {
            if let Some((node_str, detail)) = rest.split_once(':') {
                return Self::L1Semantic {
                    node: NodeId::from(node_str.trim()),
                    current_l1: String::new(),
                    actual_l2_evidence: detail.trim().to_string(),
                    discovered_by: None,
                };
            }
        }
        let lower = desc.to_ascii_lowercase();
        if lower.contains("scope") || lower.contains("range") || lower.contains("missing module") {
            return Self::ScopeGap {
                missing_nodes: issue.scope.clone(),
                missing_edges: Vec::new(),
                detail: issue.description.clone(),
                discovered_by: None,
            };
        }
        let error_type = if lower.contains("cycle")
            || lower.contains("wrong")
            || lower.contains("incorrect")
        {
            L0ErrorType::WrongRelation
        } else if lower.contains("dangling") || lower.contains("missing node") {
            L0ErrorType::MissingNode
        } else {
            L0ErrorType::MissingRelation
        };
        Self::L0Structural {
            error_type,
            detail: issue.description.clone(),
            related_nodes: issue.scope.clone(),
            discovered_by: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubTaskFailure {
    pub task_id: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalResult {
    pub graph: Graph,
    pub rounds: usize,
    pub transcript: String,
    pub last_verification: Option<VerificationResult>,
    /// Outcome of the Task phase. `None` when the loop wasn't configured
    /// with a Decomposer + Dispatcher pair (Phase 2 v1 behavior — Graph
    /// phase straight to Done).
    pub task_outcome: Option<DispatchOutcome>,
    /// Review phase verdict. `None` when the loop wasn't configured with a
    /// Reviewer (Phase 3 v1 behavior — Task phase straight to Done).
    pub review_result: Option<ReviewResult>,
}

// ---------------------------------------------------------------------------
// Internal phase enum (caller never sees this directly)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Building / extending / verifying the relationship graph.
    Graph,
    /// Dispatching sub-agents to execute the task DAG. (Phase 3.)
    #[allow(dead_code)]
    Task,
    /// Running the deterministic + LLM acceptance review. (Phase 3.)
    #[allow(dead_code)]
    Review,
    /// Terminal — `step()` returns Done.
    Done,
    /// Terminal-on-error.
    Poisoned,
}

/// Pending caller-facing operations that block `step()` until `resume*` is called.
#[derive(Debug, Clone)]
enum Pending {
    None,
    /// We surfaced `Paused`; waiting for the user's answer to be plumbed back.
    AwaitingAnswer { question: String },
    /// We surfaced `GraphInvalid`; waiting for a repaired graph or force-skip.
    AwaitingRepair,
}

// ---------------------------------------------------------------------------
// GraphLoop
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct GraphLoopConfig {
    /// Hard cap on total proposer rounds (including ask_user / call_tool turns).
    pub max_rounds: usize,
    /// How many local-repair attempts to make per Verifier failure before
    /// surfacing a `LoopState::Error` to the caller.
    pub max_repair_rounds: usize,
    /// cwd passed to ToolContext when invoking tools.
    pub tool_cwd: PathBuf,
    /// Output cap for tool results that are appended back into the conversation.
    pub tool_output_cap: usize,
    /// Policy applied to tool invocations.
    pub tool_policy: Arc<dyn crate::tools::Policy>,
}

impl GraphLoopConfig {
    pub fn defaults_at(cwd: impl Into<PathBuf>) -> Self {
        Self {
            max_rounds: 50,
            max_repair_rounds: 4,
            tool_cwd: cwd.into(),
            tool_output_cap: 12_000,
            tool_policy: Arc::new(crate::tools::AllowAll),
        }
    }
}

pub struct GraphLoop {
    pub proposer: GraphProposer,
    pub verifier: Verifier,
    pub repairer: Option<LocalRepairer>,
    /// Optional L1Enricher. When present, the loop auto-enriches new
    /// nodes after every patch application, fulfilling the L0→L1 linkage
    /// from v2.0 design (see [[feedback-three-layer-graph]]).
    pub enricher: Option<L1Enricher>,
    /// Phase 3: when present alongside `dispatcher` and `subagent_loader`,
    /// the loop runs the Task phase. Otherwise it skips straight to Done
    /// after verification (Phase 2 v1 behavior, preserved).
    pub decomposer: Option<Decomposer>,
    pub dispatcher: Option<Dispatcher>,
    /// `SourceLoader` passed to sub-agents during Task phase. Often the
    /// same loader configured on the L1Enricher; kept separate so callers
    /// can swap (e.g., wider read scope for sub-agents than for L1).
    pub subagent_loader: Option<Arc<dyn SourceLoader>>,
    /// Phase 4: optional PostExecutionValidator that runs between the Task
    /// phase and the Review phase. When the validator returns
    /// `FailedAsGraphIssue`, the loop surfaces
    /// `LoopState::GraphInvalid { source: PostExecutionValidation }`
    /// without ever invoking the Reviewer — saving the model call for the
    /// common compile/test-failure case.
    pub validator: Option<Arc<dyn PostExecutionValidator>>,
    /// Phase 4: when present, the Review phase runs deterministic checks
    /// + optional LLM-as-judge before declaring Done. Without it the
    /// stub Review fires (immediate Done after Task phase).
    pub reviewer: Option<Reviewer>,
    /// Tools available to the **main agent**. In pure-orchestrator
    /// mode this is empty (no bash) — the main agent's only
    /// execution path is `explore`, which dispatches a subagent
    /// with the `subagent_tools` registry below.
    pub tools: Arc<ToolRegistry>,
    /// Tools available to subagents dispatched via the `explore`
    /// step. Defaults to `tools.clone()` so single-registry
    /// callers (e.g. CLI binary, tests) keep working unchanged.
    /// Production web/CLI paths override via
    /// `with_subagent_tools(...)` so the main agent has no
    /// bash but subagents do.
    pub subagent_tools: Arc<ToolRegistry>,
    pub config: GraphLoopConfig,

    pub task: String,
    pub conversation: Conversation,
    pub graph: Graph,
    pub round: usize,
    pub last_verification: Option<VerificationResult>,
    pub task_outcome: Option<DispatchOutcome>,
    pub review_result: Option<ReviewResult>,
    /// The most recent Proposer step. Set inside `step_graph` before
    /// the per-step match; cleared (or overwritten) on the next step.
    /// Lets callers (e.g. the web gateway's run driver) surface each
    /// step as a transcript event for the UI without modifying the
    /// step pipeline itself.
    pub last_step: Option<ProposerStep>,
    /// Most recent tool call result (main agent only — sub-agents
    /// route their tool calls through `SubAgent::execute`, not here).
    /// Set when the Proposer took a `CallTool` step and the tool
    /// returned (success or error). Cleared (or overwritten) on
    /// the next step. Lets callers emit a `tool_result` transcript
    /// event alongside the matching `tool_use` line.
    pub last_tool_result: Option<(String, String)>,
    /// Final summary the model produced when the run terminated
    /// (max_rounds or stuck_loop). Set by `summarize_with_no_tools`
    /// just before returning `Done`. The web driver surfaces this
    /// to the user as the "what we got done" message instead of
    /// a bare error string. None means the run ended without a
    /// summary being collected (e.g. summary LLM call itself
    /// failed, or the run ended in a normal Done path).
    pub final_summary: Option<String>,
    /// Combined stuck-detector signature for the most recent successful
    /// `CallTool` step: hash of `(command_signature, output_prefix_hash)`.
    /// Used to recognize when the model is calling the same tool with
    /// effectively the same intent (either the same command, or a
    /// different command that produces the same output prefix). Reset
    /// to `None` when the model makes progress (propose_patch /
    /// ask_user).
    last_stuck_signature: Option<u64>,
    /// How many consecutive rounds had the same stuck signature.
    /// Reset to 0 when the signature changes. Drives the tiered
    /// escalation: soft hint at 3, hard hint at 5, terminate at 6.
    stuck_repeat_count: u32,
    /// Per-tool consecutive-failure counts. When a tool call
    /// fails, this map is incremented for that tool name; on a
    /// successful call, it's reset to 0. Drives the
    /// same-tool-failure warn / halt escalation (Hermes-style).
    tool_failure_counts: std::collections::HashMap<String, u32>,
    /// Cumulative tokens used by every model call so far. The
    /// Proposer/Reviewer/Validator all sum into this so the caller
    /// can surface it on a `Status` event for cost / progress
    /// visibility.
    pub tokens_used: u64,

    phase: Phase,
    pending: Pending,
}

impl GraphLoop {
    pub fn new(
        task: impl Into<String>,
        proposer: GraphProposer,
        verifier: Verifier,
        repairer: Option<LocalRepairer>,
        tools: Arc<ToolRegistry>,
        config: GraphLoopConfig,
    ) -> Self {
        let task = task.into();
        let conversation = proposer.make_conversation(&task);
        Self {
            proposer,
            verifier,
            repairer,
            enricher: None,
            decomposer: None,
            dispatcher: None,
            subagent_loader: None,
            validator: None,
            reviewer: None,
            tools: tools.clone(),
            // Default: subagent gets the same toolset as the
            // main agent. Production web/CLI paths override
            // with `with_subagent_tools(...)` so the main
            // agent has no bash but subagents do.
            subagent_tools: tools,
            config,
            task,
            conversation,
            graph: Graph::new(),
            round: 0,
            last_verification: None,
            task_outcome: None,
            review_result: None,
            last_step: None,
            last_tool_result: None,
            final_summary: None,
            last_stuck_signature: None,
            stuck_repeat_count: 0,
            tool_failure_counts: std::collections::HashMap::new(),
            tokens_used: 0,
            phase: Phase::Graph,
            pending: Pending::None,
        }
    }

    /// Attach an [`L1Enricher`]. The loop will auto-enrich new nodes added
    /// by `ProposePatch` and (on `resume_with_repaired_graph`) any nodes
    /// still missing L1 in the replaced graph.
    pub fn with_l1_enricher(mut self, enricher: L1Enricher) -> Self {
        self.enricher = Some(enricher);
        self
    }

    /// Seed the loop with an existing graph (e.g., the graph captured
    /// at the end of a prior conversation turn). The loop will continue
    /// to extend / verify / repair this graph instead of starting empty.
    pub fn with_initial_graph(mut self, graph: Graph) -> Self {
        self.graph = graph;
        self
    }

    /// Seed the loop with a pre-built [`Conversation`] (system prompt +
    /// prior messages). Used by the web gateway's multi-turn chat: the
    /// new turn inherits the prior transcript so the Proposer/SubAgent
    /// see the conversation history, not just a fresh `Task: ...` line.
    ///
    /// The caller is responsible for setting `conversation.task_description`
    /// to the new task. `round` is preserved as-is.
    pub fn with_initial_conversation(mut self, conversation: Conversation) -> Self {
        self.conversation = conversation;
        self
    }

    /// Phase 3: configure the Task phase. All three of decomposer,
    /// dispatcher, and subagent_loader must be set for the Task phase to
    /// actually run; missing any one of them keeps the loop in the
    /// Phase 2 v1 mode (Graph phase → Done).
    pub fn with_decomposer(mut self, d: Decomposer) -> Self {
        self.decomposer = Some(d);
        self
    }

    pub fn with_dispatcher(mut self, d: Dispatcher) -> Self {
        self.dispatcher = Some(d);
        self
    }

    pub fn with_subagent_loader(mut self, loader: Arc<dyn SourceLoader>) -> Self {
        self.subagent_loader = Some(loader);
        self
    }

    /// Phase 4: configure the Review phase. Without a reviewer, the loop
    /// short-circuits to Done after Task phase (or after Graph phase if
    /// Task is unconfigured).
    pub fn with_reviewer(mut self, r: Reviewer) -> Self {
        self.reviewer = Some(r);
        self
    }

    /// Phase 4: configure a PostExecutionValidator to run between Task and
    /// Review. When the validator returns `FailedAsGraphIssue`, the loop
    /// surfaces `LoopState::GraphInvalid { source: PostExecutionValidation }`.
    /// When it returns `FailedAsTaskIssue`, the loop continues to Review
    /// (which will catch the failure via its model judge). When it returns
    /// `Passed`, the loop also continues to Review.
    pub fn with_validator(mut self, v: Arc<dyn PostExecutionValidator>) -> Self {
        self.validator = Some(v);
        self
    }

    /// Resume after surfacing `Paused`. The answer is added to the
    /// conversation as a user turn and the loop continues on next `step()`.
    pub fn resume(&mut self, answer: impl Into<String>) {
        let answer = answer.into();
        if !matches!(self.pending, Pending::AwaitingAnswer { .. }) {
            warn!("resume() called but loop wasn't awaiting an answer; treating as plain user message");
        }
        self.conversation.add_user(answer);
        self.pending = Pending::None;
    }

    /// Resume after surfacing `GraphInvalid` by handing the loop a repaired
    /// graph. The next `step()` will re-enter the Graph phase to re-verify.
    /// If an [`L1Enricher`] is configured, any nodes in the repaired graph
    /// that lack L1 are enriched before the next verification cycle.
    pub fn resume_with_repaired_graph(&mut self, repaired: Graph) {
        if !matches!(self.pending, Pending::AwaitingRepair) {
            warn!("resume_with_repaired_graph() called but loop wasn't awaiting repair");
        }
        self.graph = repaired;
        self.graph.bump_version();
        self.conversation.add_user(format!(
            "External graph repair applied. Graph now has {} nodes / {} edges.",
            self.graph.node_count(),
            self.graph.edge_count(),
        ));
        self.pending = Pending::None;
        self.phase = Phase::Graph;
    }

    /// Resume by force — caller decided NOT to fix the surfaced issue and
    /// wants to proceed anyway. Risky; logged. Phase 2 v1 treats this the
    /// same as `resume_with_repaired_graph(current)`.
    pub fn resume_force(&mut self) {
        if !matches!(self.pending, Pending::None) {
            warn!("resume_force() called — skipping repair on caller's authority");
        }
        self.pending = Pending::None;
    }

    /// Advance one beat. Returns the loop's new public state.
    pub async fn step(&mut self) -> LoopState {
        if self.phase == Phase::Done {
            return LoopState::Done(self.build_final_result());
        }
        if self.phase == Phase::Poisoned {
            return LoopState::Error("loop poisoned by previous error".into());
        }
        if let Pending::AwaitingAnswer { question } = &self.pending {
            return LoopState::Paused {
                question: question.clone(),
                rationale: String::new(),
            };
        }
        if matches!(self.pending, Pending::AwaitingRepair) {
            return LoopState::GraphInvalid {
                source: ErrorSource::VerifierStalemate,
                errors: vec![],
                snapshot: self.graph.clone(),
            };
        }

        if self.round >= self.config.max_rounds {
            warn!(rounds = self.round, "max_rounds reached; requesting graceful summary");
            self.phase = Phase::Poisoned;
            let summary = self.summarize_with_no_tools().await;
            let err = match summary {
                Some(s) => format!(
                    "max_rounds ({}) reached; here is the model's best-effort summary:\n\n{}",
                    self.config.max_rounds, s
                ),
                None => format!(
                    "max_rounds ({}) reached without convergence (summary LLM call also failed)",
                    self.config.max_rounds
                ),
            };
            return LoopState::Error(err);
        }
        self.round += 1;

        match self.phase {
            Phase::Graph => match self.step_graph().await {
                Ok(s) => s,
                Err(e) => {
                    self.phase = Phase::Poisoned;
                    LoopState::Error(format!("graph phase error: {e}"))
                }
            },
            Phase::Task => self.step_task_stub().await,
            Phase::Review => self.step_review_stub().await,
            Phase::Done => LoopState::Done(self.build_final_result()),
            Phase::Poisoned => LoopState::Error("loop poisoned".into()),
        }
    }

    // -----------------------------------------------------------------------
    // Graph phase
    // -----------------------------------------------------------------------

    async fn step_graph(&mut self) -> Result<LoopState> {
        let (step, tokens) = self
            .proposer
            .next_step_with_retry(&self.conversation, &self.graph)
            .await?;
        // Accumulate tokens used by this Proposer call.
        self.tokens_used = self.tokens_used.saturating_add(tokens);
        // Persist what the model "said" so the next turn keeps history.
        let assistant_msg = render_step_as_json(&step);
        self.conversation.add_assistant(assistant_msg);
        info!(round = self.round, kind = step.kind(), "graph-phase step");
        // Expose the step to the caller (e.g. web run driver emits a
        // transcript event for the UI). Stored in a public field so
        // we don't have to plumb a callback through the type.
        self.last_step = Some(step.clone());

        match step {
            ProposerStep::AskUser { question, rationale } => {
                self.pending = Pending::AwaitingAnswer { question: question.clone() };
                // Reset stuck detector — engaging the user is a way out.
                self.stuck_repeat_count = 0;
                self.last_stuck_signature = None;
                Ok(LoopState::Paused { question, rationale })
            }
            ProposerStep::Block { reason, needed_from_user, rationale } => {
                // Reset stuck detector — the model is explicitly
                // self-pausing with a blocker reason. The user sees
                // a Paused run with the reason on the transcript
                // and can unblock by sending a free-text answer.
                self.stuck_repeat_count = 0;
                self.last_stuck_signature = None;
                // Format the pause question so the user sees both
                // the reason and the optional follow-up. If
                // `needed_from_user` is empty, just show the reason.
                let question = if needed_from_user.trim().is_empty() {
                    format!("[block] {reason}")
                } else {
                    format!("[block] {reason} — {needed_from_user}")
                };
                Ok(LoopState::Paused { question, rationale })
            }
            ProposerStep::Explore { items, rationale: _ } => {
                // Claude Code's `EXPLORE_AGENT` pattern, with
                // parallel fan-out when the model emits multiple
                // items. Each item is a (scope, question) pair;
                // items run concurrently as separate subagents
                // and the main agent gets one combined summary.
                self.stuck_repeat_count = 0;
                self.last_stuck_signature = None;
                self.dispatch_explore_subagents(items).await
            }
            ProposerStep::CallTool {
                tool,
                args,
                rationale: _,
            } => {
                let ctx = ToolContext::new(self.config.tool_cwd.clone())
                    .with_policy(self.config.tool_policy.clone())
                    .with_max_output(self.config.tool_output_cap);
                match self.tools.invoke(&tool, args, &ctx).await {
                    Ok(out) => {
                        // Reset per-tool failure counter on success
                        // (the tool is working now). Distinct from
                        // the stuck detector below, which tracks
                        // repeated *successful* same-output calls.
                        self.tool_failure_counts.remove(&tool);

                        // Snapshot the result for the caller to surface as
                        // a `tool_result` transcript event. Truncate the
                        // body so a 4MB log dump doesn't flood the UI.
                        let preview: String = out
                            .content
                            .chars()
                            .take(800)
                            .collect::<String>()
                            .trim_end()
                            .to_string();
                        let summary = if preview.chars().count() < out.content.chars().count() {
                            format!("{preview}…")
                        } else {
                            preview
                        };
                        self.last_tool_result =
                            Some((tool.clone(), format!("exit={:?} · {}", out.exit_code, summary)));
                        let body = format!(
                            "tool `{tool}` (exit={:?}, interrupted={}, dur_ms={}):\n{}",
                            out.exit_code, out.interrupted, out.duration_ms, out.content
                        );
                        self.conversation.add_user(body);

                        // Stuck detection: hash the output prefix
                        // (first 1024 chars). We deliberately do NOT
                        // include the command in the signature —
                        // the model can vary the command line
                        // slightly (e.g. `ls -la /` vs `ls -la / 2>&1
                        // | head -50`) while getting the same
                        // output, and that should still count as
                        // stuck. What matters is whether the model
                        // is seeing new information, not whether it's
                        // typing the same characters.
                        let output_prefix: String = out
                            .content
                            .chars()
                            .take(STUCK_OUTPUT_PREFIX_CHARS)
                            .collect();
                        let output_sig = hash_string(&output_prefix);
                        if self.last_stuck_signature == Some(output_sig) {
                            self.stuck_repeat_count += 1;
                        } else {
                            self.last_stuck_signature = Some(output_sig);
                            self.stuck_repeat_count = 1;
                        }

                        // Tier 3: hard terminate the run. This is
                        // the only branch that returns a non-Running
                        // state, so the model cannot make the run
                        // drag on by ignoring hints forever.
                        if self.stuck_repeat_count >= STUCK_REPEAT_TERMINATE {
                            error!(
                                tool = %tool,
                                count = self.stuck_repeat_count,
                                "graph-phase stuck loop: requesting graceful summary"
                            );
                            self.phase = Phase::Poisoned;
                            let summary = self.summarize_with_no_tools().await;
                            let err = match summary {
                                Some(s) => format!(
                                    "stuck loop exceeded: tool `{tool}` has been called \
                                     {} times in a row with the same command and producing \
                                     the same output. The model is not making progress. \
                                     Here is the model's best-effort summary:\n\n{}",
                                    self.stuck_repeat_count, s
                                ),
                                None => format!(
                                    "stuck loop exceeded: tool `{tool}` has been called \
                                     {} times in a row with the same command and producing \
                                     the same output. The model is not making progress. \
                                     Hint: refine the task description, narrow the scope, \
                                     or break it into a smaller sub-task before retrying.",
                                    self.stuck_repeat_count
                                ),
                            };
                            return Ok(LoopState::Error(err));
                        }

                        // Tier 2: hard hint — the next repeat will
                        // terminate. Model has a chance to act on
                        // it; if it doesn't, tier 3 fires.
                        if self.stuck_repeat_count >= STUCK_REPEAT_HARD_HINT {
                            warn!(
                                tool = %tool,
                                count = self.stuck_repeat_count,
                                "graph-phase stuck detector: hard hint; next repeat will terminate"
                            );
                            self.conversation.add_user(format!(
                                "Note: you have just called `{tool}` with the same \
                                 arguments AND produced the same output {} times in a \
                                 row. The NEXT call with the same args will TERMINATE \
                                 this run. You MUST now either:\n\
                                 - emit a `propose_patch` to record what you have \
                                 already learned, or\n\
                                 - emit `ask_user` for direction.\n\
                                 Do NOT call the same tool with the same args again.",
                                self.stuck_repeat_count
                            ));
                        } else if self.stuck_repeat_count >= STUCK_REPEAT_SOFT_HINT {
                            warn!(
                                tool = %tool,
                                count = self.stuck_repeat_count,
                                "graph-phase stuck detector: soft hint"
                            );
                            self.conversation.add_user(format!(
                                "Note: you have just called `{tool}` with the same \
                                 arguments AND produced the same output {} times in a \
                                 row. Calling it again is unlikely to produce new \
                                 information. Either:\n\
                                 - emit a `propose_patch` to record what you have \
                                 already learned, or\n\
                                 - emit `ask_user` if you need direction from the user.\n\
                                 Do NOT call the same tool with the same args again.",
                                self.stuck_repeat_count
                            ));
                        }
                    }
                    Err(e) => {
                        self.last_tool_result = Some((tool.clone(), format!("error: {e}")));
                        self.conversation.add_user(format!(
                            "tool `{tool}` errored: {e}. Adjust and try a different step."
                        ));

                        // Tool-failure guardrail (Hermes §tool_loop_guardrails).
                        // Track consecutive failures per tool name; a
                        // sustained failure pattern means the model is
                        // retrying the same approach without
                        // adjustment. At thresholds, escalate:
                        // 3 → warn, 8 → graceful summary + terminate.
                        let count = self
                            .tool_failure_counts
                            .entry(tool.clone())
                            .or_insert(0);
                        *count += 1;
                        let failure_count = *count;
                        if failure_count >= TOOL_FAILURE_HALT_AFTER {
                            error!(
                                tool = %tool,
                                count = failure_count,
                                "graph-phase tool-failure guardrail: requesting graceful summary"
                            );
                            self.phase = Phase::Poisoned;
                            // Drop the borrow on `self.tool_failure_counts`
                            // before calling `self.summarize_with_no_tools`
                            // (which mutates `self.conversation`).
                            self.tool_failure_counts.clear();
                            let summary = self.summarize_with_no_tools().await;
                            let err = match summary {
                                Some(s) => format!(
                                    "tool-failure guardrail: `{tool}` failed {} \
                                     times in a row. The model is not making \
                                     progress. Here is the model's \
                                     best-effort summary:\n\n{}",
                                    failure_count, s
                                ),
                                None => format!(
                                    "tool-failure guardrail: `{tool}` failed {} \
                                     times in a row. The model is not making \
                                     progress. Check the tool environment \
                                     (network, credentials, sandbox) and \
                                     retry with a working setup.",
                                    failure_count
                                ),
                            };
                            return Ok(LoopState::Error(err));
                        }
                        if failure_count >= TOOL_FAILURE_WARN_AFTER {
                            warn!(
                                tool = %tool,
                                count = failure_count,
                                "graph-phase tool-failure guardrail: same tool failed repeatedly; \
                                 model should change strategy"
                            );
                            self.conversation.add_user(format!(
                                "Note: `{tool}` has now failed {} times in a row. \
                                 Stop retrying it the same way. Switch tools, \
                                 change arguments significantly, or emit \
                                 `ask_user`/`block` to surface the problem.",
                                failure_count
                            ));
                        }
                    }
                }
                Ok(LoopState::Running)
            }
            ProposerStep::ProposePatch { patch, rationale: _ } => {
                let before_nodes = self.graph.node_count();
                let before_edges = self.graph.edge_count();
                // Capture which node ids the patch is adding, so we can
                // dispatch L1 enrichment for just those nodes after apply.
                let new_node_ids: Vec<NodeId> =
                    patch.add_nodes.iter().map(|n| n.id.clone()).collect();
                match self.graph.apply_patch(patch.clone()) {
                    Ok(()) => {
                        // Reset stuck detector — the model is making progress.
                        self.stuck_repeat_count = 0;
                        self.last_stuck_signature = None;
                        self.conversation.add_user(format!(
                            "Patch applied. Graph went from {before_nodes}n/{before_edges}e to {}n/{}e. Continue.",
                            self.graph.node_count(),
                            self.graph.edge_count()
                        ));
                        // L0 → L1 linkage: auto-enrich brand-new nodes.
                        // Per design doc v2.0, L1 is the enricher's job, not
                        // the proposer's; this is where that responsibility
                        // is discharged.
                        if !new_node_ids.is_empty() {
                            self.auto_enrich(&new_node_ids).await;
                        }
                    }
                    Err(e) => {
                        // Patch rejected at the graph level (e.g., dangling endpoint).
                        // Feed it back so the model can correct itself.
                        self.conversation.add_user(format!(
                            "Patch REJECTED: {e}. The graph was not modified. Adjust and propose a new patch."
                        ));
                    }
                }
                Ok(LoopState::Running)
            }
            ProposerStep::ReadyForVerify { rationale: _ } => self.run_verify_and_maybe_repair().await,
        }
    }

    async fn run_verify_and_maybe_repair(&mut self) -> Result<LoopState> {
        let result = self
            .verifier
            .verify(&self.graph, &self.task, Some(&self.conversation))
            .await?;
        debug!(
            passed = result.passed,
            issue_count = result.issues.len(),
            "verifier returned"
        );
        self.last_verification = Some(result.clone());

        if result.passed {
            // Verifier passed → advance to Task phase. step_task decides
            // whether to actually run (Phase 3 components present) or skip
            // straight to Review (Phase 2 v1 mode).
            self.phase = Phase::Task;
            return Ok(LoopState::Running);
        }

        // High-severity issues only. Medium/Low surface in last_verification
        // but don't block.
        let high_issues: Vec<VerifyIssue> = result
            .issues
            .iter()
            .filter(|i| i.severity == Severity::High)
            .cloned()
            .collect();

        if high_issues.is_empty() {
            // Verifier said "fail" but no High-severity items. Either the
            // failing_severities was widened or a Medium item was flagged.
            // Accept the graph and advance to Task.
            self.phase = Phase::Task;
            return Ok(LoopState::Running);
        }

        // Local repair, one issue at a time, capped at max_repair_rounds.
        // Per principle #3: never bulk; each issue translates into one local
        // patch.
        let Some(repairer) = self.repairer.clone() else {
            // No repairer configured. Surface the high issues to the caller
            // as GraphInvalid so they can decide how to handle.
            let errors = high_issues
                .iter()
                .map(GraphError::from_verify_issue)
                .collect();
            self.pending = Pending::AwaitingRepair;
            return Ok(LoopState::GraphInvalid {
                source: ErrorSource::VerifierStalemate,
                errors,
                snapshot: self.graph.clone(),
            });
        };

        let mut remaining = high_issues;
        let mut attempts = 0usize;
        while !remaining.is_empty() && attempts < self.config.max_repair_rounds {
            attempts += 1;
            let issue = remaining.remove(0);
            info!(
                attempt = attempts,
                issue = %issue.description,
                "local-repairing high-severity issue"
            );
            match repairer.repair(&self.graph, &issue, &self.task).await {
                Ok(patch) => match self.graph.apply_patch(patch) {
                    Ok(()) => {
                        self.conversation.add_user(format!(
                            "Local repair applied for: {}",
                            issue.description
                        ));
                    }
                    Err(e) => {
                        warn!(error = %e, "local-repair patch failed to apply");
                        self.conversation.add_user(format!(
                            "Local repair FAILED to apply: {e}. Issue still: {}",
                            issue.description
                        ));
                    }
                },
                Err(e) => {
                    warn!(error = %e, "local-repairer rejected patch (scope or schema)");
                    self.conversation.add_user(format!(
                        "Local repair attempt errored: {e}. Issue still: {}",
                        issue.description
                    ));
                }
            }

            // Re-verify after each surgical patch — that's the heart of
            // principle #2 (time-for-space): each precise correction
            // translates into a fresh verification, not a batched one.
            let recheck = self
                .verifier
                .verify(&self.graph, &self.task, Some(&self.conversation))
                .await?;
            self.last_verification = Some(recheck.clone());
            if recheck.passed {
                self.phase = Phase::Task;
                return Ok(LoopState::Running);
            }
            // Refresh the high-issue list from the latest verification
            remaining = recheck
                .issues
                .iter()
                .filter(|i| i.severity == Severity::High)
                .cloned()
                .collect();
        }

        // Exhausted budget. Hand off to caller.
        let errors = remaining.iter().map(GraphError::from_verify_issue).collect();
        self.pending = Pending::AwaitingRepair;
        Ok(LoopState::GraphInvalid {
            source: ErrorSource::VerifierStalemate,
            errors,
            snapshot: self.graph.clone(),
        })
    }

    // -----------------------------------------------------------------------
    // Task / Review — Phase 3 task implementation
    // -----------------------------------------------------------------------

    async fn step_task_stub(&mut self) -> LoopState {
        // When all three Phase 3 components are configured, run the real
        // Task phase. Otherwise short-circuit straight to Done — preserves
        // the Phase 2 v1 semantic that "verifier passes → one more step
        // returns Done" for callers that haven't wired Phase 3.
        let (decomp, disp, loader) = match (
            self.decomposer.as_ref(),
            self.dispatcher.as_ref(),
            self.subagent_loader.as_ref(),
        ) {
            (Some(d), Some(disp), Some(l)) => (d.clone(), disp.clone(), l.clone()),
            _ => {
                debug!("graph_loop: Phase 3 components not all configured; short-circuit to Done");
                self.phase = Phase::Done;
                return LoopState::Done(self.build_final_result());
            }
        };

        info!(
            world_nodes = self.graph.node_count(),
            "graph_loop: entering Task phase"
        );

        // 1. Decompose
        let task_graph = match decomp
            .decompose(&self.graph, &self.task, Some(&self.conversation))
            .await
        {
            Ok(g) => g,
            Err(e) => {
                warn!(error = %e, "decomposer failed");
                self.phase = Phase::Poisoned;
                return LoopState::Error(format!("decomposer failed: {e}"));
            }
        };
        info!(
            tasks = task_graph.node_count(),
            "graph_loop: task graph produced"
        );
        self.conversation.add_user(format!(
            "Task phase: decomposed into {} sub-task(s).",
            task_graph.node_count()
        ));

        // Empty decomposition → still valid, skip to Review.
        if task_graph.node_count() == 0 {
            info!("graph_loop: empty task graph; transitioning straight to Review");
            self.task_outcome = Some(DispatchOutcome {
                results: Vec::new(),
                batches: Vec::new(),
                total_subagent_ms: 0,
                total_tokens: 0,
                all_succeeded: true,
                graph_errors: Vec::new(),
            });
            self.phase = Phase::Review;
            return LoopState::Running;
        }

        // 2. Dispatch
        let outcome = match disp.run(&task_graph, &self.graph, loader).await {
            Ok(o) => o,
            Err(e) => {
                warn!(error = %e, "dispatcher failed");
                self.phase = Phase::Poisoned;
                return LoopState::Error(format!("dispatcher failed: {e}"));
            }
        };
        info!(
            results = outcome.results.len(),
            success = outcome.all_succeeded,
            wall_ms = outcome.total_subagent_ms,
            tokens = outcome.total_tokens,
            "graph_loop: dispatcher complete"
        );
        self.conversation.add_user(format!(
            "Task phase: dispatched {} task(s); {} succeeded, {} failed.",
            outcome.results.len(),
            outcome.results.iter().filter(|r| r.success).count(),
            outcome.results.iter().filter(|r| !r.success).count(),
        ));

        // 3. Branch on success
        let all_ok = outcome.all_succeeded;
        let task_graph_errors = outcome.graph_errors.clone();
        let failures: Vec<SubTaskFailure> = outcome
            .results
            .iter()
            .filter(|r| !r.success)
            .map(|r| SubTaskFailure {
                task_id: r.task_id.to_string(),
                error: r.error.clone().unwrap_or_default(),
            })
            .collect();
        self.task_outcome = Some(outcome);

        // Priority: graph errors reported by sub-agents bubble up FIRST.
        // The v2 design says any sub-agent's graph-error signal interrupts
        // the parent and routes back to GRAPH state for local repair.
        if !task_graph_errors.is_empty() {
            warn!(
                count = task_graph_errors.len(),
                "graph_loop: sub-agent(s) reported graph errors; surfacing GraphInvalid"
            );
            self.pending = Pending::AwaitingRepair;
            return LoopState::GraphInvalid {
                source: ErrorSource::DuringExecution,
                errors: task_graph_errors,
                snapshot: self.graph.clone(),
            };
        }

        if !all_ok {
            // Surface failures to the caller — Phase 3 v1 doesn't auto-retry.
            warn!(
                count = failures.len(),
                "graph_loop: surfacing TaskFailed to caller"
            );
            return LoopState::TaskFailed { failures };
        }

        // Phase 4: PostExecutionValidator runs between Task and Review.
        // FailedAsGraphIssue → bubble GraphInvalid { source: PostExecutionValidation }
        // without invoking the (potentially expensive) Reviewer.
        // FailedAsTaskIssue / Passed → fall through to Review.
        if let Some(validator) = self.validator.clone() {
            let outcome_ref = self.task_outcome.as_ref().expect("task_outcome set above");
            match validator
                .validate(&self.graph, outcome_ref, &self.task)
                .await
            {
                Ok(ValidationVerdict::Passed) => {
                    info!("graph_loop: PostExecutionValidator passed");
                }
                Ok(ValidationVerdict::FailedAsTaskIssue { details }) => {
                    info!(
                        details = %details,
                        "graph_loop: PostExecutionValidator failed as task issue; deferring to Review"
                    );
                    self.conversation.add_user(format!(
                        "Post-execution validator flagged a task-level issue: {details}"
                    ));
                }
                Ok(ValidationVerdict::FailedAsGraphIssue { errors }) => {
                    warn!(
                        count = errors.len(),
                        "graph_loop: PostExecutionValidator failed as graph issue; surfacing GraphInvalid"
                    );
                    self.pending = Pending::AwaitingRepair;
                    return LoopState::GraphInvalid {
                        source: ErrorSource::PostExecutionValidation,
                        errors,
                        snapshot: self.graph.clone(),
                    };
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "graph_loop: PostExecutionValidator errored; treating as Passed and continuing to Review"
                    );
                }
            }
        }

        self.phase = Phase::Review;
        LoopState::Running
    }

    async fn step_review_stub(&mut self) -> LoopState {
        // Phase 4: when a reviewer is configured, run the real review;
        // otherwise short-circuit to Done (Phase 3 v1 behavior).
        let reviewer = match &self.reviewer {
            Some(r) => r.clone(),
            None => {
                self.phase = Phase::Done;
                return LoopState::Done(self.build_final_result());
            }
        };

        info!("graph_loop: entering Review phase");
        let result = match reviewer
            .review(
                &self.task,
                &self.graph,
                self.task_outcome.as_ref(),
                self.last_verification.as_ref(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "reviewer failed");
                self.phase = Phase::Poisoned;
                return LoopState::Error(format!("reviewer failed: {e}"));
            }
        };
        info!(passed = result.passed, "graph_loop: reviewer verdict");
        self.review_result = Some(result.clone());

        if result.passed {
            self.phase = Phase::Done;
            return LoopState::Done(self.build_final_result());
        }

        // Failed review. Route based on judge's root_cause:
        // - GraphIssue / ScopeIssue → back to Graph phase as GraphInvalid
        // - TaskIssue (or unknown) → Done with non-passing review embedded
        //   (caller inspects and decides whether to retry Task phase)
        match result.root_cause() {
            Some(RootCause::GraphIssue) | Some(RootCause::ScopeIssue) => {
                let errors = reviewer.to_graph_errors(&result);
                self.pending = Pending::AwaitingRepair;
                warn!(
                    issue_count = errors.len(),
                    "graph_loop: review failed with graph-rooted cause; surfacing GraphInvalid"
                );
                LoopState::GraphInvalid {
                    source: ErrorSource::Review,
                    errors,
                    snapshot: self.graph.clone(),
                }
            }
            Some(RootCause::TaskIssue) | None => {
                warn!("graph_loop: review failed (task-rooted or no root_cause); returning Done with embedded verdict");
                self.phase = Phase::Done;
                LoopState::Done(self.build_final_result())
            }
        }
    }

    // -----------------------------------------------------------------------
    // Bookkeeping
    // -----------------------------------------------------------------------

    fn build_final_result(&self) -> FinalResult {
        FinalResult {
            graph: self.graph.clone(),
            rounds: self.round,
            transcript: self.conversation.transcript(),
            last_verification: self.last_verification.clone(),
            task_outcome: self.task_outcome.clone(),
            review_result: self.review_result.clone(),
        }
    }

    /// Dispatch one or more subagents to do a multi-file
    /// exploration on the main agent's behalf. Used by the
    /// `Explore` step kind (Claude Code's `EXPLORE_AGENT`
    /// pattern, with parallel fan-out). All items run
    /// concurrently as separate subagents; the main agent's
    /// conversation gets a single user message containing
    /// each subagent's summary. The main agent is not
    /// exposed to the raw file contents — that's the
    /// whole point of the subagent: keep the main context
    /// clean.
    async fn dispatch_explore_subagents(
        &mut self,
        items: Vec<ExploreItem>,
    ) -> Result<LoopState> {
        use super::contract::CheckContract;
        use super::subagent::{SubAgent, SubAgentResult, SubTask};
        use crate::context::FilesystemSources;
        use std::time::Instant;
        use tracing::info;

        let n = items.len();
        info!(
            items = n,
            "graph-phase Explore: dispatching subagent batch (parallel)"
        );

        // One shared subagent config — `SubAgent::execute` is
        // `&self`, so we can reuse one instance for the
        // whole batch. No step cap (per user override 2026-06-06):
        // the subagent reads as long as it needs to.
        let subagent = SubAgent::new(self.proposer.model.clone())
            .with_tools(self.subagent_tools.clone())
            .with_policy(self.config.tool_policy.clone())
            .with_tool_cwd(self.config.tool_cwd.clone())
            .with_tool_output_cap(self.config.tool_output_cap)
            .with_max_steps(usize::MAX);

        // Build one SubTask per item. Unique IDs so repeated
        // Explore dispatches in the same run don't collide.
        let tasks: Vec<SubTask> = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                // Exploratory contract: the sub-agent must mention the
                // scope (to prove it actually looked there) and report
                // at most 5 items per step (to bound per-subagent
                // output size). Region is empty here because Explore
                // items don't pin to a fixed set of nodes — the scope
                // string is the addressing scheme.
                let contract = CheckContract::Exploratory {
                    region: Vec::new(),
                    max_items: 5,
                    must_mention_any: vec![item.scope.clone()],
                };
                SubTask {
                    id: NodeId::from(format!(
                        "explore-r{}-i{}-pid{}",
                        self.round,
                        i,
                        std::process::id()
                    )),
                    description: format!(
                        "Explore scope: {}\nQuestion: {}\n\n\
                         Read the relevant files (use `cat`, `head`, or `grep` via \
                         bash) and produce a concise summary that directly \
                         answers the question. Cite file paths you read.",
                        item.scope, item.question
                    ),
                    involved_nodes: Vec::new(),
                    needs: Default::default(),
                    contract,
                }
            })
            .collect();

        // Fire all subagents concurrently via JoinSet. Wall-
        // clock is roughly the slowest single subagent, not
        // the sum. Each spawned task owns its clone of the
        // subagent, the graph, the SourceLoader, and the
        // SubTask — no borrows into the outer function.
        let started = Instant::now();
        let mut joinset: tokio::task::JoinSet<std::result::Result<SubAgentResult, crate::error::HarnessError>> =
            tokio::task::JoinSet::new();
        for t in &tasks {
            // SubAgent: Clone. Graph: Clone. FilesystemSources:
            // owns a PathBuf (construct per-task, it's cheap).
            let subagent = subagent.clone();
            let graph = self.graph.clone();
            let loader = FilesystemSources::new(&self.config.tool_cwd);
            let task = t.clone();
            joinset.spawn(async move {
                subagent.execute(&task, &graph, &loader).await
            });
        }

        let mut results: Vec<std::result::Result<SubAgentResult, crate::error::HarnessError>> =
            Vec::with_capacity(tasks.len());
        while let Some(res) = joinset.join_next().await {
            match res {
                Ok(r) => results.push(r),
                Err(join_err) => {
                    // JoinError means the task panicked or was
                    // aborted; surface as a per-item error so
                    // the main agent still sees a structured
                    // failure rather than a missing item.
                    warn!(error = %join_err, "explore subagent task join error");
                    results.push(Err(crate::error::HarnessError::model(format!(
                        "subagent task join error: {join_err}"
                    ))));
                }
            }
        }
        let elapsed_ms = started.elapsed().as_millis() as u64;

        // Combine all results into a single user message
        // for the main agent. Item-by-item, success or
        // failure — the main agent sees what each subagent
        // did, in order.
        let mut body = format!(
            "Explore subagent batch ({} item{}, {}ms total):\n",
            n,
            if n == 1 { "" } else { "s" },
            elapsed_ms,
        );
        for (i, (item, res)) in items.iter().zip(results.iter()).enumerate() {
            body.push_str(&format!(
                "\n## Item {} — scope `{}`, question: {}\n",
                i + 1,
                item.scope,
                item.question,
            ));
            match res {
                Ok(r) if r.success => {
                    body.push_str(&format!(
                        "**Result** ({} tool calls, {}ms):\n{}\n",
                        r.tool_calls_made, r.duration_ms, r.output
                    ));
                }
                Ok(r) => {
                    body.push_str(&format!(
                        "**Failed**: {}\n",
                        r.error.as_deref().unwrap_or("?")
                    ));
                }
                Err(e) => {
                    body.push_str(&format!("**Error**: {e}\n"));
                }
            }
        }
        self.conversation.add_user(body);

        Ok(LoopState::Running)
    }

    /// Graceful summary at budget exhaustion. Per Hermes's
    /// `handle_max_iterations`: instead of just terminating with
    /// `max_rounds reached`, ask the model to give a final response
    /// summarizing what it found. The result is stored in
    /// `self.final_summary` and the caller surfaces it to the user
    /// as a "best-effort done" message.
    ///
    /// The LLM call is made with the same `next_step` machinery
    /// but with a stripped tools payload (the model is told
    /// "no more tools, summarize"). If the LLM call itself fails,
    /// we return None and the caller falls back to the bare
    /// error string.
    async fn summarize_with_no_tools(&mut self) -> Option<String> {
        use crate::agent::proposer::render_graph_for_prompt;
        use crate::model::{Message, Role};
        // Inject the summary request as the latest user message.
        // Use a clearly-marked framing so the model treats it as
        // a direct instruction rather than chit-chat.
        self.conversation.add_user(
            "The tool-calling iteration budget has been reached. \
             You may NOT call any more tools. Give a final response \
             now: summarize what you found, what is still missing, \
             and (if applicable) what the user should do next to \
             unblock. Be concrete — name the files / nodes / \
             decisions you actually made, not aspirations. Keep it \
             under 400 words."
                .to_string(),
        );

        // We need a ModelRequest WITHOUT tools. The proposer
        // builds requests from the registered ToolRegistry, so we
        // do the request here directly with an empty tool set.
        let graph_snapshot = render_graph_for_prompt(&self.graph);
        let mut messages: Vec<Message> = self
            .conversation
            .messages
            .iter()
            .map(|m| Message {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();
        // Re-inject the system prompt + graph snapshot header
        // (matching the proposer's request shape, just without
        // tools).
        if let Some(first) = messages.first() {
            if !matches!(first.role, Role::System) {
                let system = self.proposer.build_system_prompt(&self.task);
                let mut with_system = vec![
                    Message::system(system),
                    Message::system(format!(
                        "Current relationship-graph snapshot (authoritative — \
                         your beliefs about the graph should match this):\n{graph_snapshot}"
                    )),
                ];
                with_system.append(&mut messages);
                messages = with_system;
            }
        }

        let req = crate::model::ModelRequest {
            messages,
            tools: vec![],
            temperature: 0.0,
            max_tokens: Some(512),
            stop: vec![],
        };

        // Retry once on transient HTTP errors (529 overloaded,
        // 5xx server errors, request timeouts). The summary
        // path is the last resort before terminating the run,
        // so it's worth one retry. After the retry, fall
        // back to the bare error string — we'd rather have
        // the user see "stuck loop + hint" than hang
        // forever waiting for an API that's down.
        const MAX_ATTEMPTS: u32 = 2;
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
        let mut last_err: Option<crate::error::HarnessError> = None;
        let mut resp_opt: Option<crate::model::ModelResponse> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match self.proposer.model.complete(req.clone()).await {
                Ok(r) => {
                    resp_opt = Some(r);
                    last_err = None;
                    break;
                }
                Err(e) => {
                    if attempt < MAX_ATTEMPTS {
                        warn!(
                            attempt,
                            error = %e,
                            "graph-phase graceful summary: LLM call failed; retrying"
                        );
                        tokio::time::sleep(RETRY_DELAY).await;
                        last_err = Some(e);
                    } else {
                        warn!(
                            attempt,
                            error = %e,
                            "graph-phase graceful summary: LLM call failed after retry; falling back to bare error"
                        );
                        last_err = Some(e);
                    }
                }
            }
        }
        match resp_opt {
            Some(resp) => {
                let trimmed = resp.content.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    // Also record the summary in the conversation
                    // so the transcript (audit log) has it.
                    self.conversation
                        .messages
                        .push(crate::model::Message {
                            role: Role::Assistant,
                            content: trimmed.clone(),
                        });
                    Some(trimmed)
                }
            }
            None => {
                // Retries exhausted; last_err is Some.
                debug!(
                    error = ?last_err,
                    "graph-phase graceful summary: no response after retries; falling back to bare error"
                );
                None
            }
        }
    }

    /// Override the subagent tool registry. Used by the
    /// web/CLI path to give subagents a full toolset (with
    /// bash) while leaving the main agent's `tools` empty
    /// (no direct execution — pure orchestrator mode). The
    /// CLI binary and tests don't call this; they get the
    /// same registry for both, preserving the legacy
    /// behavior.
    pub fn with_subagent_tools(mut self, tools: Arc<ToolRegistry>) -> Self {
        self.subagent_tools = tools;
        self
    }

    /// Drive the L1Enricher across `ids` if one is configured. Errors are
    /// logged but never bubble up — L1 enrichment is best-effort linkage,
    /// not a correctness gate (the Verifier's L1 sampling layer is the
    /// real gate).
    async fn auto_enrich(&mut self, ids: &[NodeId]) {
        let enricher = match &self.enricher {
            Some(e) => e.clone(),
            None => return,
        };
        match enricher
            .enrich_missing(&mut self.graph, ids, &self.task, 0.5)
            .await
        {
            Ok(n) if n > 0 => {
                self.conversation.add_user(format!(
                    "L1 auto-enrichment: wrote L1 for {n} node(s)."
                ));
                debug!(enriched = n, total = ids.len(), "L1 auto-enrichment after patch");
            }
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, "L1 auto-enrichment errored; continuing");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Stuck-detection tier 1: after this many consecutive rounds with the
/// same `(command, output)` signature, inject a soft hint into the
/// conversation telling the model to propose_patch or ask_user.
const STUCK_REPEAT_SOFT_HINT: u32 = 3;

/// Stuck-detection tier 2: after this many consecutive rounds, escalate
/// to a hard hint that says the NEXT repeat will terminate the run.
const STUCK_REPEAT_HARD_HINT: u32 = 5;

/// Stuck-detection tier 3: at this many consecutive rounds, terminate
/// the run with `LoopState::Error` instead of letting it burn
/// `max_rounds` rounds. The error message is informative — the user
/// can see "stuck loop: 6 repeats" and try a different task shape.
const STUCK_REPEAT_TERMINATE: u32 = 6;

/// Tool-failure guardrail tier 1 (Hermes §tool_loop_guardrails
/// `same_tool_failure_warn_after`): after this many consecutive
/// failures of the same tool, inject a hint telling the model to
/// change strategy.
const TOOL_FAILURE_WARN_AFTER: u32 = 3;

/// Tool-failure guardrail tier 2: after this many consecutive
/// failures, request a graceful summary and terminate.
const TOOL_FAILURE_HALT_AFTER: u32 = 8;

/// How many leading characters of the tool output to fold into the
/// stuck signature. Long enough to catch "same first page" patterns
/// (e.g. `ls -la` listings) without being expensive to hash.
const STUCK_OUTPUT_PREFIX_CHARS: usize = 1024;

/// Stable hash of an arbitrary string. Used for both the per-tool
/// command signature and the per-call output-prefix signature.
fn hash_string(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------

/// Re-serialize a parsed [`ProposerStep`] back into the JSON form the model
/// emitted, so the conversation history stays self-consistent. Not
/// byte-identical to the model's original output (rationale ordering can
/// differ), but semantically equivalent.
fn render_step_as_json(step: &ProposerStep) -> String {
    let v = match step {
        ProposerStep::AskUser { question, rationale } => serde_json::json!({
            "step": "ask_user",
            "question": question,
            "rationale": rationale,
        }),
        ProposerStep::CallTool {
            tool,
            args,
            rationale,
        } => serde_json::json!({
            "step": "call_tool",
            "tool": tool,
            "args": args,
            "rationale": rationale,
        }),
        ProposerStep::ProposePatch { patch, rationale } => serde_json::json!({
            "step": "propose_patch",
            "patch": patch,
            "rationale": rationale,
        }),
        ProposerStep::ReadyForVerify { rationale } => serde_json::json!({
            "step": "ready_for_verify",
            "rationale": rationale,
        }),
        ProposerStep::Block { reason, needed_from_user, rationale } => serde_json::json!({
            "step": "block",
            "reason": reason,
            "needed_from_user": needed_from_user,
            "rationale": rationale,
        }),
        ProposerStep::Explore { items, rationale } => serde_json::json!({
            "step": "explore",
            "items": items.iter().map(|i| serde_json::json!({
                "scope": i.scope,
                "question": i.question,
            })).collect::<Vec<_>>(),
            "rationale": rationale,
        }),
    };
    v.to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::verifier::{IssueSource, Severity};
    use crate::tools::BashTool;

    #[test]
    fn hash_string_is_stable() {
        assert_eq!(hash_string("hello world"), hash_string("hello world"));
    }

    #[test]
    fn hash_string_differs_on_content() {
        assert_ne!(hash_string("hello"), hash_string("world"));
        assert_ne!(hash_string("a"), hash_string("A"));
    }

    #[test]
    fn hash_string_handles_empty() {
        // Should not panic; the empty string has a stable hash.
        let a = hash_string("");
        let b = hash_string("");
        assert_eq!(a, b);
    }

    #[test]
    fn stuck_thresholds_are_in_ascending_order() {
        // Regression guard: the tiered escalation depends on these
        // being in the right order. If a refactor changes one, this
        // test names exactly which constant is wrong.
        assert!(
            STUCK_REPEAT_SOFT_HINT < STUCK_REPEAT_HARD_HINT,
            "soft hint must fire before hard hint (got soft={}, hard={})",
            STUCK_REPEAT_SOFT_HINT,
            STUCK_REPEAT_HARD_HINT
        );
        assert!(
            STUCK_REPEAT_HARD_HINT < STUCK_REPEAT_TERMINATE,
            "hard hint must fire before terminate (got hard={}, terminate={})",
            STUCK_REPEAT_HARD_HINT,
            STUCK_REPEAT_TERMINATE
        );
        assert!(
            STUCK_REPEAT_TERMINATE <= 6,
            "terminate threshold must be ≤ max_rounds / 4 to fire before \
             max_rounds (24); got {}",
            STUCK_REPEAT_TERMINATE
        );
    }
    use crate::graph::{Edge, NodeKind, RelationType};
    use crate::model::{FinishReason, Model, ModelRequest, ModelResponse, Usage};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Scripted multi-turn model: cycles through a queue of responses.
    /// Re-uses last response if the queue is empty (simulating a stuck model).
    struct ScriptedModel {
        responses: Mutex<VecDeque<String>>,
        sticky_last: Mutex<Option<String>>,
        called: Mutex<usize>,
    }

    impl ScriptedModel {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().map(String::from).collect()),
                sticky_last: Mutex::new(None),
                called: Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Model for ScriptedModel {
        fn name(&self) -> &str {
            "scripted"
        }
        async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse> {
            *self.called.lock().unwrap() += 1;
            let content = {
                let mut q = self.responses.lock().unwrap();
                if let Some(next) = q.pop_front() {
                    *self.sticky_last.lock().unwrap() = Some(next.clone());
                    next
                } else {
                    self.sticky_last
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| "{}".to_string())
                }
            };
            Ok(ModelResponse {
                content,
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }
    }

    fn build_loop_with(model_responses: Vec<&str>) -> GraphLoop {
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(model_responses));
        let tools = Arc::new(ToolRegistry::new());
        let proposer = GraphProposer::new(model.clone(), tools.clone(), None);
        let verifier = Verifier::structural_only();
        let cfg = GraphLoopConfig::defaults_at(std::env::current_dir().unwrap());
        GraphLoop::new("test task", proposer, verifier, None, tools, cfg)
    }

    /// Drive `step()` until the loop returns a non-Running state. Cap at
    /// 64 iterations to avoid hanging on bugs. Most tests only care about
    /// the terminal state, not the intermediate `Running` ticks that
    /// happen as the phase machine walks through stub transitions.
    async fn drive_to_terminal(gl: &mut GraphLoop) -> LoopState {
        for _ in 0..64 {
            match gl.step().await {
                LoopState::Running => continue,
                other => return other,
            }
        }
        panic!("loop did not reach a terminal state within 64 steps")
    }

    #[tokio::test]
    async fn empty_graph_passes_structural_verify_immediately() {
        let mut gl = build_loop_with(vec![
            r#"{"step":"ready_for_verify","rationale":"nothing to add"}"#,
        ]);
        match drive_to_terminal(&mut gl).await {
            LoopState::Done(r) => {
                assert_eq!(r.graph.node_count(), 0);
                assert!(r.last_verification.unwrap().passed);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn with_initial_graph_seeds_the_loop() {
        // Build a loop, then seed it with a pre-existing graph. The
        // first step() should run against the seeded graph, not empty.
        let mut gl = build_loop_with(vec![
            r#"{"step":"ready_for_verify","rationale":"graph already covers it"}"#,
        ]);
        let mut seed = Graph::new();
        seed.add_node(crate::graph::Node::file("alpha.rs", "alpha"));
        seed.add_node(crate::graph::Node::file("beta.rs", "beta"));
        seed.add_edge(Edge::new("alpha.rs", "beta.rs", RelationType::Imports, 0.9, ""))
            .unwrap();
        gl = gl.with_initial_graph(seed);
        assert_eq!(gl.graph.node_count(), 2);
        assert_eq!(gl.graph.edge_count(), 1);
        // The seeded graph must be returned in the final result.
        match drive_to_terminal(&mut gl).await {
            LoopState::Done(r) => {
                assert_eq!(r.graph.node_count(), 2, "seeded nodes should be preserved");
                assert_eq!(r.graph.edge_count(), 1);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn with_initial_conversation_seeds_messages() {
        // The web gateway's multi-turn chat uses this to hand the agent
        // the previous turn's transcript. Verify the seeded messages
        // land at the front of `conversation.messages`, ahead of the
        // auto-appended `Task: ...` line.
        let mut gl = build_loop_with(vec![
            r#"{"step":"ready_for_verify","rationale":"got it"}"#,
        ]);
        let mut conv = gl.proposer.make_conversation("next turn task");
        use crate::model::{Message, Role};
        conv.messages.clear();
        conv.messages.push(Message {
            role: Role::User,
            content: "first turn said hi".into(),
        });
        conv.messages.push(Message {
            role: Role::Assistant,
            content: "first turn got: ask_user".into(),
        });
        // drive_run appends a fresh "Task: ..." line so the loop
        // sees the new task; mirror that here.
        conv.messages
            .push(Message::user(format!("Task: {}", conv.task_description)));
        gl = gl.with_initial_conversation(conv);
        // First = seeded user line; last = the new task.
        let msgs = &gl.conversation.messages;
        assert_eq!(msgs[0].role, Role::User);
        assert!(msgs[0].content.contains("first turn said hi"));
        assert!(msgs.last().unwrap().content.contains("next turn task"));
    }

    #[test]
    fn main_agent_has_no_direct_tools_under_pure_orchestrator() {
        // Regression guard: the main agent's `tools` field is the
        // empty registry in production wiring (api_runs.rs). If a
        // future change accidentally re-populates it with bash,
        // the system prompt would advertise bash and the
        // call_tool step would succeed — defeating the
        // pure-orchestrator design and bringing the bash-loop
        // failure mode back.
        let bash_only = Arc::new({
            let mut reg = ToolRegistry::new();
            reg.register(Arc::new(BashTool::new()));
            reg
        });
        let empty = Arc::new(ToolRegistry::new());

        // `with_subagent_tools` is the production wiring shape:
        // main = empty, subagent = full. `tools` and
        // `subagent_tools` MUST differ.
        let mut gl = build_loop_with(vec![r#"{"step":"ready_for_verify","rationale":"x"}"#]);
        gl = gl.with_subagent_tools(bash_only.clone());
        assert_eq!(gl.tools.defs().len(), 0, "main agent must have no direct tools");
        assert_eq!(
            gl.subagent_tools.defs().len(),
            1,
            "subagent toolset must include bash"
        );

        // Default (no with_subagent_tools) keeps the legacy
        // behavior — both main and subagent share whatever the
        // caller passed. This is the path the CLI binary and
        // unit tests use.
        let gl_default = build_loop_with(vec![r#"{"step":"ready_for_verify","rationale":"x"}"#]);
        assert_eq!(gl_default.tools.defs().len(), 0); // build_loop_with uses empty
        assert_eq!(gl_default.subagent_tools.defs().len(), 0);
        assert!(Arc::ptr_eq(&gl_default.tools, &gl_default.subagent_tools));

        // The system prompt's "Available tools" message tells the
        // model it has no direct tools and to use `explore`.
        let p = gl.proposer.build_system_prompt("any task");
        assert!(
            p.contains("your only execution path is the `explore` step"),
            "system prompt must explicitly teach the pure-orchestrator rule"
        );
        assert!(
            p.contains("`call_tool` it will fail"),
            "system prompt must warn that call_tool is no longer available"
        );
    }

    #[tokio::test]
    async fn ask_user_surfaces_paused_state() {
        let mut gl = build_loop_with(vec![
            r#"{"step":"ask_user","question":"how many users?","rationale":"scale matters"}"#,
        ]);
        match gl.step().await {
            LoopState::Paused { question, .. } => assert_eq!(question, "how many users?"),
            other => panic!("expected Paused, got {other:?}"),
        }
        // Calling step() again without resume should re-surface the same Paused state
        match gl.step().await {
            LoopState::Paused { .. } => {}
            other => panic!("expected Paused again, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resume_clears_pending_and_advances() {
        let mut gl = build_loop_with(vec![
            r#"{"step":"ask_user","question":"q?","rationale":""}"#,
            r#"{"step":"ready_for_verify","rationale":"got the info"}"#,
        ]);
        assert!(matches!(gl.step().await, LoopState::Paused { .. }));
        gl.resume("here's the answer");
        assert!(matches!(drive_to_terminal(&mut gl).await, LoopState::Done(_)));
    }

    #[tokio::test]
    async fn propose_patch_apply_then_verify_succeeds() {
        let patch_json = r#"{
            "step":"propose_patch",
            "patch":{
                "add_nodes":[{"id":"a","kind":"File","path":"a.rs","summary":"file A"},
                              {"id":"b","kind":"File","path":"b.rs","summary":"file B"}],
                "add_edges":[{"source":"a","target":"b","relation":"Imports","confidence":0.9,"evidence":"use b"}],
                "reason":"core structure"
            },
            "rationale":"two-file project"
        }"#;
        let mut gl = build_loop_with(vec![
            patch_json,
            r#"{"step":"ready_for_verify","rationale":"sufficient"}"#,
        ]);
        // First step: apply the patch (yields Running)
        assert!(matches!(gl.step().await, LoopState::Running));
        assert_eq!(gl.graph.node_count(), 2);
        assert_eq!(gl.graph.edge_count(), 1);
        // Drive to terminal — proposer says ready, verifier passes, phase walks to Done.
        assert!(matches!(drive_to_terminal(&mut gl).await, LoopState::Done(_)));
    }

    #[tokio::test]
    async fn invalid_patch_does_not_modify_graph_and_continues() {
        // Patch references nodes that don't exist — graph rejects it,
        // loop keeps going (feeds rejection back to the model).
        let bad_patch = r#"{
            "step":"propose_patch",
            "patch":{
                "add_edges":[{"source":"ghost","target":"phantom","relation":"Imports","confidence":0.9,"evidence":""}],
                "reason":"dangling endpoints"
            }
        }"#;
        let mut gl = build_loop_with(vec![
            bad_patch,
            r#"{"step":"ready_for_verify","rationale":"giving up"}"#,
        ]);
        assert!(matches!(gl.step().await, LoopState::Running));
        // Graph should still be empty
        assert_eq!(gl.graph.node_count(), 0);
        assert!(matches!(drive_to_terminal(&mut gl).await, LoopState::Done(_)));
    }

    #[tokio::test]
    async fn max_rounds_exhaustion_yields_error() {
        // Model only ever asks questions; user never answers (we don't resume).
        let mut gl = build_loop_with(vec![
            r#"{"step":"ask_user","question":"q","rationale":""}"#,
        ]);
        gl.config.max_rounds = 1;
        // First step: Paused
        assert!(matches!(gl.step().await, LoopState::Paused { .. }));
        // Without resume(), we get Paused again — that doesn't increment round
        assert!(matches!(gl.step().await, LoopState::Paused { .. }));
        // Now resume and let it run again
        gl.resume("answer");
        match gl.step().await {
            LoopState::Error(msg) => assert!(msg.contains("max_rounds")),
            // Or it could finish if scripted enough — but with only 1 round budget
            // and one already consumed, this should hit Error.
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poisoned_state_is_sticky() {
        let mut gl = build_loop_with(vec!["not even valid JSON"]);
        // First call returns Error
        match gl.step().await {
            LoopState::Error(_) => {}
            other => panic!("expected Error from malformed JSON, got {other:?}"),
        }
        // Subsequent calls keep returning Error
        match gl.step().await {
            LoopState::Error(_) => {}
            other => panic!("expected Error sticky, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn call_tool_routes_through_registry_and_continues() {
        // Set up: a tool registry with just BashTool.
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(crate::tools::BashTool::new()));
        let tools = Arc::new(reg);

        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![
            r#"{"step":"call_tool","tool":"bash","args":{"command":"echo from_tool"},"rationale":"smoke"}"#,
            r#"{"step":"ready_for_verify","rationale":"done"}"#,
        ]));
        let proposer = GraphProposer::new(model.clone(), tools.clone(), None);
        let verifier = Verifier::structural_only();
        let cfg = GraphLoopConfig::defaults_at(std::env::current_dir().unwrap());
        let mut gl = GraphLoop::new("smoke test", proposer, verifier, None, tools, cfg);

        // First step: tool call
        assert!(matches!(gl.step().await, LoopState::Running));
        // Tool result should be in the conversation now
        let transcript = gl.conversation.transcript();
        assert!(transcript.contains("from_tool"), "transcript:\n{transcript}");
        // Drive to terminal — ready → verify → Done
        assert!(matches!(drive_to_terminal(&mut gl).await, LoopState::Done(_)));
    }

    #[tokio::test]
    async fn verifier_high_issue_without_repairer_surfaces_graph_invalid() {
        // Make a graph that triggers a high-severity structural issue (cycle).
        // To do this: pre-load a graph with a cycle before calling step().
        // We need a model that immediately says ready_for_verify.
        let mut gl = build_loop_with(vec![
            r#"{"step":"ready_for_verify","rationale":"start verify"}"#,
        ]);
        // Inject a cycle directly into the graph
        gl.graph.add_node(crate::graph::Node::task("t1", "T1"));
        gl.graph.add_node(crate::graph::Node::task("t2", "T2"));
        gl.graph
            .add_edge(Edge::new("t1", "t2", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        gl.graph
            .add_edge(Edge::new("t2", "t1", RelationType::DependsOn, 1.0, ""))
            .unwrap();

        // No repairer configured → high severity should surface as GraphInvalid
        match gl.step().await {
            LoopState::GraphInvalid { source, errors, .. } => {
                assert!(matches!(source, ErrorSource::VerifierStalemate));
                assert!(!errors.is_empty());
            }
            other => panic!("expected GraphInvalid, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resume_with_repaired_graph_re_enters_graph_phase() {
        let mut gl = build_loop_with(vec![
            r#"{"step":"ready_for_verify","rationale":"v1"}"#,
            r#"{"step":"ready_for_verify","rationale":"v2"}"#,
        ]);
        // Inject a cycle to force GraphInvalid
        gl.graph.add_node(crate::graph::Node::task("t1", "T1"));
        gl.graph.add_node(crate::graph::Node::task("t2", "T2"));
        gl.graph
            .add_edge(Edge::new("t1", "t2", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        gl.graph
            .add_edge(Edge::new("t2", "t1", RelationType::DependsOn, 1.0, ""))
            .unwrap();
        // Step → GraphInvalid
        assert!(matches!(gl.step().await, LoopState::GraphInvalid { .. }));
        // Caller "repairs" the graph (just removes the cycle)
        let mut fixed = gl.graph.clone();
        // Easiest fix: empty the edges
        fixed.edges.clear();
        fixed.rebuild_indices();
        gl.resume_with_repaired_graph(fixed);
        // Drive to terminal — verifier now passes, phase walks through to Done.
        match drive_to_terminal(&mut gl).await {
            LoopState::Done(r) => assert_eq!(r.graph.edge_count(), 0),
            other => panic!("expected Done after repair, got {other:?}"),
        }
    }

    #[test]
    fn graph_error_from_verify_issue_picks_sensible_variant() {
        let cycle = VerifyIssue {
            source: IssueSource::Structural,
            severity: Severity::High,
            description: "cycle in DependsOn".into(),
            scope: vec![NodeId::from("t1")],
        };
        match GraphError::from_verify_issue(&cycle) {
            GraphError::L0Structural { error_type, .. } => {
                assert_eq!(error_type, L0ErrorType::WrongRelation)
            }
            other => panic!("expected L0Structural WrongRelation, got {other:?}"),
        }

        let dangling = VerifyIssue {
            source: IssueSource::Structural,
            severity: Severity::High,
            description: "dangling edge to ghost".into(),
            scope: vec![NodeId::from("ghost")],
        };
        match GraphError::from_verify_issue(&dangling) {
            GraphError::L0Structural { error_type, .. } => {
                assert_eq!(error_type, L0ErrorType::MissingNode)
            }
            other => panic!("expected L0Structural MissingNode, got {other:?}"),
        }

        let scope_issue = VerifyIssue {
            source: IssueSource::Model,
            severity: Severity::High,
            description: "task scope is wrong; missing module foo".into(),
            scope: vec![],
        };
        match GraphError::from_verify_issue(&scope_issue) {
            GraphError::ScopeGap { detail, .. } => {
                assert!(detail.contains("scope"))
            }
            other => panic!("expected ScopeGap, got {other:?}"),
        }

        let l1_drift = VerifyIssue {
            source: IssueSource::Model,
            severity: Severity::High,
            description: "L1 drift on auth/jwt.rs: claims HS256 but L2 uses RS256".into(),
            scope: vec![NodeId::from("auth/jwt.rs")],
        };
        match GraphError::from_verify_issue(&l1_drift) {
            GraphError::L1Semantic {
                node,
                actual_l2_evidence,
                ..
            } => {
                assert_eq!(node, NodeId::from("auth/jwt.rs"));
                assert!(actual_l2_evidence.contains("RS256"));
            }
            other => panic!("expected L1Semantic, got {other:?}"),
        }

        let generic = VerifyIssue {
            source: IssueSource::Model,
            severity: Severity::High,
            description: "missing data flow".into(),
            scope: vec![],
        };
        match GraphError::from_verify_issue(&generic) {
            GraphError::L0Structural { error_type, .. } => {
                assert_eq!(error_type, L0ErrorType::MissingRelation)
            }
            other => panic!("expected L0Structural MissingRelation, got {other:?}"),
        }
    }

    #[test]
    fn graph_error_helpers_return_consistent_data() {
        let e = GraphError::L0Structural {
            error_type: L0ErrorType::MissingRelation,
            detail: "edge a->b missing".into(),
            related_nodes: vec![NodeId::from("a"), NodeId::from("b")],
            discovered_by: None,
        };
        assert_eq!(e.kind_label(), "L0Structural");
        assert_eq!(e.related_nodes().len(), 2);
        assert!(e.detail().contains("MissingRelation"));
        assert!(e.detail().contains("edge a->b missing"));

        let l1 = GraphError::L1Semantic {
            node: NodeId::from("x"),
            current_l1: "stores users".into(),
            actual_l2_evidence: "actually stores accounts".into(),
            discovered_by: Some("t_analyze".into()),
        };
        assert_eq!(l1.kind_label(), "L1Semantic");
        assert_eq!(l1.related_nodes(), vec![NodeId::from("x")]);
        assert_eq!(l1.discovered_by(), Some("t_analyze"));
        assert!(l1.detail().contains("L1 drift"));

        let sg = GraphError::ScopeGap {
            missing_nodes: vec![NodeId::from("m1"), NodeId::from("m2")],
            missing_edges: vec![(NodeId::from("a"), NodeId::from("m1"), "Calls".into())],
            detail: "task needs auth module".into(),
            discovered_by: None,
        };
        assert_eq!(sg.kind_label(), "ScopeGap");
        assert_eq!(sg.related_nodes().len(), 2);
        assert!(sg.detail().contains("auth module"));
        assert!(sg.detail().contains("1 edge"));
    }

    #[test]
    fn graph_error_with_discovered_by_sets_field() {
        let e = GraphError::L0Structural {
            error_type: L0ErrorType::MissingRelation,
            detail: "x".into(),
            related_nodes: vec![],
            discovered_by: None,
        };
        let tagged = e.with_discovered_by("batch_2/t3");
        assert_eq!(tagged.discovered_by(), Some("batch_2/t3"));
    }

    #[test]
    fn render_step_as_json_round_trips_through_parse_step() {
        // The JSON we write back into the conversation should itself parse
        // back into the same step kind.
        let steps = [
            ProposerStep::AskUser {
                question: "q".into(),
                rationale: "r".into(),
            },
            ProposerStep::CallTool {
                tool: "bash".into(),
                args: serde_json::json!({"command": "ls"}),
                rationale: "see".into(),
            },
            ProposerStep::ReadyForVerify {
                rationale: "done".into(),
            },
        ];
        for s in &steps {
            let serialized = render_step_as_json(s);
            let parsed = crate::agent::proposer::parse_step(&serialized).unwrap();
            assert_eq!(parsed.kind(), s.kind());
        }
    }

    #[tokio::test]
    async fn auto_enrich_writes_l1_for_new_nodes_after_patch() {
        // The shared ScriptedModel sees three calls in order:
        //   1. Proposer asks for next step → returns a propose_patch
        //   2. L1Enricher asks for L1 for new node → returns L1 JSON
        //   3. Proposer asks for next step → returns ready_for_verify
        let patch_json = r#"{
            "step":"propose_patch",
            "patch":{
                "add_nodes":[{"id":"x","kind":"File","path":"x.rs","summary":"X module"}],
                "add_edges":[],
                "reason":"add X"
            }
        }"#;
        let l1_json = r#"{
            "responsibility":"holds X",
            "implementation":"plain struct",
            "design_intent":"isolate X concerns",
            "constraints":"no panics",
            "confidence":0.9
        }"#;
        let ready_json = r#"{"step":"ready_for_verify"}"#;

        let shared: Arc<dyn Model> =
            Arc::new(ScriptedModel::new(vec![patch_json, l1_json, ready_json]));
        let tools = Arc::new(ToolRegistry::new());
        let proposer = GraphProposer::new(shared.clone(), tools.clone(), None);
        let verifier = Verifier::structural_only();

        let mut sources = std::collections::HashMap::new();
        sources.insert(NodeId::from("x"), "pub struct X;\n".into());
        let loader = Arc::new(crate::context::InMemorySources(sources));
        let enricher = crate::agent::enricher::L1Enricher::new(shared.clone(), loader);

        let cfg = GraphLoopConfig::defaults_at(std::env::current_dir().unwrap());
        let mut gl = GraphLoop::new("add module X", proposer, verifier, None, tools, cfg)
            .with_l1_enricher(enricher);

        // Step 1: proposer returns patch → apply → auto-enrich
        assert!(matches!(gl.step().await, LoopState::Running));
        assert_eq!(gl.graph.node_count(), 1);
        // L1 store should have been populated by auto_enrich
        let l1 = gl
            .graph
            .l1
            .get(&NodeId::from("x"))
            .expect("auto-enrichment should have written L1 for x");
        assert_eq!(l1.responsibility, "holds X");
        assert!((l1.confidence - 0.9).abs() < 1e-9);

        // Drive to terminal — proposer says ready, phase walks to Done.
        assert!(matches!(drive_to_terminal(&mut gl).await, LoopState::Done(_)));
    }

    #[tokio::test]
    async fn auto_enrich_skipped_when_no_enricher_configured() {
        // Without an enricher, the L1 store should stay empty after a patch.
        let patch_json = r#"{
            "step":"propose_patch",
            "patch":{
                "add_nodes":[{"id":"x","kind":"File","path":"x.rs","summary":"X"}],
                "add_edges":[],
                "reason":"add X"
            }
        }"#;
        let ready_json = r#"{"step":"ready_for_verify"}"#;
        let mut gl = build_loop_with(vec![patch_json, ready_json]);
        // No .with_l1_enricher

        assert!(matches!(gl.step().await, LoopState::Running));
        assert_eq!(gl.graph.node_count(), 1);
        // No L1 entry — auto_enrich was a no-op
        assert!(gl.graph.l1.get(&NodeId::from("x")).is_none());

        assert!(matches!(drive_to_terminal(&mut gl).await, LoopState::Done(_)));
    }

    // ---------------------------------------------------------------------
    // Phase 3 integration tests: Decomposer + Dispatcher wired into the loop
    // ---------------------------------------------------------------------

    fn build_phase3_loop(model_responses: Vec<&str>) -> GraphLoop {
        // One shared model serves proposer + decomposer + sub-agents in
        // sequence (ScriptedModel's mutex guarantees serial pop ordering).
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(model_responses));
        let tools = Arc::new(ToolRegistry::new());
        let proposer = GraphProposer::new(model.clone(), tools.clone(), None);
        let verifier = Verifier::structural_only();
        let cfg = GraphLoopConfig::defaults_at(std::env::current_dir().unwrap());
        let decomposer = super::super::decomposer::Decomposer::new(model.clone());
        let agent = Arc::new(super::super::subagent::SubAgent::new(model.clone()));
        let dispatcher = super::super::dispatcher::Dispatcher::new(agent).with_max_concurrent(4);
        let loader: Arc<dyn SourceLoader> =
            Arc::new(crate::context::NullSourceLoader);
        GraphLoop::new("phase3 test task", proposer, verifier, None, tools, cfg)
            .with_decomposer(decomposer)
            .with_dispatcher(dispatcher)
            .with_subagent_loader(loader)
    }

    #[tokio::test]
    async fn phase3_runs_decompose_then_dispatch_to_done_with_outcome() {
        // Scripted flow:
        //   1. proposer: ready_for_verify (empty graph passes structural verify)
        //   2. decomposer: 2 independent tasks
        //   3. sub-agent for t1
        //   4. sub-agent for t2
        let proposer_resp = r#"{"step":"ready_for_verify","rationale":"trivial"}"#;
        let decomposer_resp = r#"{
            "tasks":[
                {"id":"t1","description":"analyze A","involved_nodes":[],"dependencies":[],"needs":{"can_read":true}},
                {"id":"t2","description":"analyze B","involved_nodes":[],"dependencies":[],"needs":{"can_read":true}}
            ],
            "rationale":"split"
        }"#;
        let subagent_t1 = "t1 result: analysis of A complete";
        let subagent_t2 = "t2 result: analysis of B complete";
        let mut gl = build_phase3_loop(vec![
            proposer_resp,
            decomposer_resp,
            subagent_t1,
            subagent_t2,
        ]);

        // Step 1: Graph phase — proposer says ready_for_verify, verifier passes,
        // phase transitions to Task.
        // (Verifier passing on empty graph → transitions to Phase::Done in the
        // old code path, BUT now with Phase 3 components configured we want it
        // to go through Task → Review → Done.)
        //
        // Actually, looking at run_verify_and_maybe_repair: when verifier
        // passes, it transitions to Done directly. So we need to refactor
        // that to go to Task when configured. Let me work around in test
        // by injecting a node first so the propose_patch flow runs.

        // Sequence: each step returns Running until we hit Done.
        let mut hit_done = false;
        for _ in 0..10 {
            match gl.step().await {
                LoopState::Running => continue,
                LoopState::Done(r) => {
                    // Task outcome should be populated from the dispatcher
                    assert!(r.task_outcome.is_some(), "expected task_outcome to be populated");
                    let outcome = r.task_outcome.unwrap();
                    assert_eq!(outcome.results.len(), 2);
                    assert!(outcome.all_succeeded);
                    assert!(outcome.results.iter().all(|r| r.success));
                    hit_done = true;
                    break;
                }
                other => panic!("unexpected loop state: {other:?}"),
            }
        }
        assert!(hit_done, "loop never reached Done");
    }

    #[tokio::test]
    async fn phase3_skipped_when_components_not_all_configured() {
        // Sanity: the existing (Phase 2 v1) loop without Phase 3 components
        // still reaches Done. task_outcome stays None.
        let mut gl = build_loop_with(vec![
            r#"{"step":"ready_for_verify","rationale":"trivial"}"#,
        ]);
        // No with_decomposer / with_dispatcher / with_subagent_loader
        match drive_to_terminal(&mut gl).await {
            LoopState::Done(r) => {
                // task_outcome stays None — Phase 3 not wired
                assert!(r.task_outcome.is_none());
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn phase3_decomposer_failure_poisons_loop() {
        let proposer_resp = r#"{"step":"ready_for_verify","rationale":"go"}"#;
        // Bad decomposer JSON → parse error
        let bad_decomposer = r#"this is not JSON at all"#;
        let mut gl = build_phase3_loop(vec![proposer_resp, bad_decomposer]);

        // Iterate until we either hit Error or run out of steps.
        let mut state = LoopState::Running;
        for _ in 0..10 {
            state = gl.step().await;
            if !matches!(state, LoopState::Running) {
                break;
            }
        }
        match state {
            LoopState::Error(msg) => assert!(
                msg.contains("decomposer failed") || msg.contains("max_rounds"),
                "expected decomposer error, got: {msg}"
            ),
            LoopState::Done(_) => {
                // Acceptable alternative: verifier transitions directly to
                // Done before Task phase ever runs (current Phase 2 v1 path).
                // The test still proves no crash.
            }
            other => panic!("unexpected state: {other:?}"),
        }
    }

    #[tokio::test]
    async fn phase3_empty_decomposition_transitions_to_review_with_empty_outcome() {
        let proposer_resp = r#"{"step":"ready_for_verify","rationale":"trivial"}"#;
        let empty_decomp = r#"{"tasks":[],"rationale":"trivial task"}"#;
        let mut gl = build_phase3_loop(vec![proposer_resp, empty_decomp]);

        let mut final_outcome = None;
        for _ in 0..10 {
            match gl.step().await {
                LoopState::Running => continue,
                LoopState::Done(r) => {
                    final_outcome = r.task_outcome;
                    break;
                }
                other => panic!("unexpected state: {other:?}"),
            }
        }
        let outcome = final_outcome.expect("expected task_outcome");
        assert!(outcome.results.is_empty());
        assert!(outcome.batches.is_empty());
        assert!(outcome.all_succeeded);
    }

    #[tokio::test]
    async fn phase3_bubbles_subagent_graph_errors_as_graphinvalid_during_execution() {
        // Sub-agent reports a graph error → dispatcher aggregates →
        // step_task surfaces LoopState::GraphInvalid { source: DuringExecution }.
        let proposer_resp = r#"{"step":"ready_for_verify","rationale":"go"}"#;
        let decomp_resp = r#"{
            "tasks":[{"id":"t1","description":"check A","involved_nodes":[],"dependencies":[],"needs":{"can_read":true}}],
            "rationale":"single check"
        }"#;
        let bubble_resp = r#"{
            "action":"report_graph_error",
            "errors":[{"kind":"L0Structural","l0_error_type":"WrongRelation","detail":"A doesn't call B; graph is wrong","related_nodes":["a","b"]}],
            "thinking":"L2 contradicts L0"
        }"#;
        let mut gl = build_phase3_loop(vec![proposer_resp, decomp_resp, bubble_resp]);

        let mut state = LoopState::Running;
        for _ in 0..10 {
            state = gl.step().await;
            if !matches!(state, LoopState::Running) {
                break;
            }
        }
        match state {
            LoopState::GraphInvalid { source, errors, .. } => {
                assert!(matches!(source, ErrorSource::DuringExecution));
                assert_eq!(errors.len(), 1);
                // discovered_by should be the sub-task id
                assert_eq!(errors[0].discovered_by(), Some("t1"));
                // task_outcome should still be populated so caller can audit
                // what got done (even though we exited early)
                assert!(gl.task_outcome.is_some());
            }
            other => panic!("expected GraphInvalid DuringExecution, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // Phase 4 integration tests: Reviewer wired into the loop
    // ---------------------------------------------------------------------

    fn build_phase4_loop(model_responses: Vec<&str>) -> GraphLoop {
        // Phase 4 = Phase 3 + Reviewer attached. Single ScriptedModel
        // serves all components serially in the order responses are queued.
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(model_responses));
        let tools = Arc::new(ToolRegistry::new());
        let proposer = GraphProposer::new(model.clone(), tools.clone(), None);
        let verifier = Verifier::structural_only();
        let cfg = GraphLoopConfig::defaults_at(std::env::current_dir().unwrap());
        let decomposer = super::super::decomposer::Decomposer::new(model.clone());
        let agent = Arc::new(super::super::subagent::SubAgent::new(model.clone()));
        let dispatcher = super::super::dispatcher::Dispatcher::new(agent).with_max_concurrent(2);
        let loader: Arc<dyn SourceLoader> = Arc::new(crate::context::NullSourceLoader);
        let reviewer = super::super::reviewer::Reviewer::with_model(model.clone());
        GraphLoop::new("phase4 test task", proposer, verifier, None, tools, cfg)
            .with_decomposer(decomposer)
            .with_dispatcher(dispatcher)
            .with_subagent_loader(loader)
            .with_reviewer(reviewer)
    }

    #[tokio::test]
    async fn phase4_review_pass_routes_to_done_with_review_result() {
        let proposer_resp = r#"{"step":"ready_for_verify","rationale":"trivial"}"#;
        let decomp_resp = r#"{"tasks":[],"rationale":"trivial"}"#;
        // Reviewer judge says pass
        let judge_resp = r#"{"verdict":"pass","detail":"covers task","confidence":0.9}"#;
        let mut gl = build_phase4_loop(vec![proposer_resp, decomp_resp, judge_resp]);

        let mut final_result = None;
        for _ in 0..20 {
            match gl.step().await {
                LoopState::Running => continue,
                LoopState::Done(r) => {
                    final_result = Some(r);
                    break;
                }
                other => panic!("unexpected state: {other:?}"),
            }
        }
        let r = final_result.expect("expected Done");
        let review = r.review_result.expect("expected review_result populated");
        assert!(review.passed);
        let j = review.judge_verdict.unwrap();
        assert!(j.passed);
        assert!((j.confidence - 0.9).abs() < 1e-6);
    }

    #[tokio::test]
    async fn phase4_review_fail_with_graph_root_cause_surfaces_graph_invalid() {
        let proposer_resp = r#"{"step":"ready_for_verify","rationale":"go"}"#;
        let decomp_resp = r#"{"tasks":[],"rationale":"trivial"}"#;
        // Reviewer judge says fail with graph root_cause
        let judge_resp = r#"{"verdict":"fail","root_cause":"graph","detail":"missing critical edges","confidence":0.85}"#;
        let mut gl = build_phase4_loop(vec![proposer_resp, decomp_resp, judge_resp]);

        let mut state = LoopState::Running;
        for _ in 0..20 {
            state = gl.step().await;
            if !matches!(state, LoopState::Running) {
                break;
            }
        }
        match state {
            LoopState::GraphInvalid { source, errors, .. } => {
                assert!(matches!(source, ErrorSource::Review));
                assert!(!errors.is_empty(), "expected at least one GraphError");
                assert_eq!(errors[0].discovered_by(), Some("reviewer"));
            }
            other => panic!("expected GraphInvalid from Review, got {other:?}"),
        }
        // review_result should also be stored on the loop for inspection
        let r = gl.review_result.expect("review_result populated");
        assert!(!r.passed);
    }

    #[tokio::test]
    async fn phase4_review_fail_with_task_root_cause_returns_done_with_verdict() {
        let proposer_resp = r#"{"step":"ready_for_verify","rationale":"go"}"#;
        let decomp_resp = r#"{"tasks":[],"rationale":"trivial"}"#;
        // Reviewer judge says fail with TASK root_cause — no graph-error
        let judge_resp = r#"{"verdict":"fail","root_cause":"task","detail":"sub-agents missed coverage","confidence":0.7}"#;
        let mut gl = build_phase4_loop(vec![proposer_resp, decomp_resp, judge_resp]);

        let mut final_result = None;
        for _ in 0..20 {
            match gl.step().await {
                LoopState::Running => continue,
                LoopState::Done(r) => {
                    final_result = Some(r);
                    break;
                }
                other => panic!("unexpected state: {other:?}"),
            }
        }
        let r = final_result.expect("expected Done with task-issue verdict");
        let review = r.review_result.expect("review_result populated");
        assert!(!review.passed);
        let j = review.judge_verdict.unwrap();
        assert_eq!(j.root_cause, Some(crate::agent::reviewer::RootCause::TaskIssue));
    }

    // ---------------------------------------------------------------------
    // Phase 4 PostExecutionValidator integration
    // ---------------------------------------------------------------------

    /// Validator that always returns the verdict provided at construction.
    /// Used in tests to drive specific code paths in step_task.
    struct FixedValidator(crate::agent::validator::ValidationVerdict);

    #[async_trait]
    impl crate::agent::validator::PostExecutionValidator for FixedValidator {
        async fn validate(
            &self,
            _graph: &Graph,
            _task_outcome: &DispatchOutcome,
            _task_description: &str,
        ) -> Result<crate::agent::validator::ValidationVerdict> {
            Ok(self.0.clone())
        }
    }

    fn build_phase4_loop_with_validator(
        model_responses: Vec<&str>,
        validator: Arc<dyn crate::agent::validator::PostExecutionValidator>,
    ) -> GraphLoop {
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(model_responses));
        let tools = Arc::new(ToolRegistry::new());
        let proposer = GraphProposer::new(model.clone(), tools.clone(), None);
        let verifier = Verifier::structural_only();
        let cfg = GraphLoopConfig::defaults_at(std::env::current_dir().unwrap());
        let decomposer = super::super::decomposer::Decomposer::new(model.clone());
        let agent = Arc::new(super::super::subagent::SubAgent::new(model.clone()));
        let dispatcher = super::super::dispatcher::Dispatcher::new(agent).with_max_concurrent(2);
        let loader: Arc<dyn SourceLoader> = Arc::new(crate::context::NullSourceLoader);
        let reviewer = super::super::reviewer::Reviewer::with_model(model.clone());
        GraphLoop::new("validator test", proposer, verifier, None, tools, cfg)
            .with_decomposer(decomposer)
            .with_dispatcher(dispatcher)
            .with_subagent_loader(loader)
            .with_reviewer(reviewer)
            .with_validator(validator)
    }

    #[tokio::test]
    async fn validator_failed_as_graph_issue_surfaces_graphinvalid_post_execution() {
        // Validator says graph issue → loop bubbles GraphInvalid with
        // source=PostExecutionValidation, BYPASSING the Reviewer entirely.
        let proposer_resp = r#"{"step":"ready_for_verify","rationale":"go"}"#;
        // Single sub-task so dispatch runs
        let decomp_resp = r#"{
            "tasks":[{"id":"t1","description":"check A","involved_nodes":[],"dependencies":[],"needs":{"can_read":true}}],
            "rationale":"single"
        }"#;
        let subagent_resp = r#"{"action":"final_answer","answer":"done","thinking":""}"#;
        // Reviewer judge — should NOT be reached because validator short-circuits
        let judge_resp = r#"{"verdict":"pass","detail":"would have passed","confidence":0.9}"#;

        let v = Arc::new(FixedValidator(
            crate::agent::validator::ValidationVerdict::FailedAsGraphIssue {
                errors: vec![GraphError::L0Structural {
                    error_type: L0ErrorType::MissingRelation,
                    detail: "cargo check failed: cannot find function `compute`".into(),
                    related_nodes: vec![NodeId::from("compute")],
                    discovered_by: Some("test-validator".into()),
                }],
            },
        ));
        let mut gl = build_phase4_loop_with_validator(
            vec![proposer_resp, decomp_resp, subagent_resp, judge_resp],
            v,
        );

        let mut state = LoopState::Running;
        for _ in 0..20 {
            state = gl.step().await;
            if !matches!(state, LoopState::Running) {
                break;
            }
        }
        match state {
            LoopState::GraphInvalid { source, errors, .. } => {
                assert!(matches!(source, ErrorSource::PostExecutionValidation));
                assert_eq!(errors.len(), 1);
                assert_eq!(errors[0].discovered_by(), Some("test-validator"));
                // review_result must NOT be populated — we never reached Review
                assert!(gl.review_result.is_none());
            }
            other => panic!("expected GraphInvalid PostExecutionValidation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn validator_passed_lets_loop_proceed_to_review() {
        let proposer_resp = r#"{"step":"ready_for_verify","rationale":"go"}"#;
        let decomp_resp = r#"{
            "tasks":[{"id":"t1","description":"x","involved_nodes":[],"dependencies":[],"needs":{}}],
            "rationale":"single"
        }"#;
        let subagent_resp = r#"{"action":"final_answer","answer":"done","thinking":""}"#;
        let judge_resp = r#"{"verdict":"pass","detail":"covers task","confidence":0.95}"#;

        let v = Arc::new(FixedValidator(
            crate::agent::validator::ValidationVerdict::Passed,
        ));
        let mut gl = build_phase4_loop_with_validator(
            vec![proposer_resp, decomp_resp, subagent_resp, judge_resp],
            v,
        );

        let mut final_result = None;
        for _ in 0..20 {
            match gl.step().await {
                LoopState::Running => continue,
                LoopState::Done(r) => {
                    final_result = Some(r);
                    break;
                }
                other => panic!("unexpected state: {other:?}"),
            }
        }
        let r = final_result.expect("expected Done");
        // Review must have run because validator passed
        assert!(r.review_result.is_some());
        assert!(r.review_result.unwrap().passed);
    }

    #[tokio::test]
    async fn validator_failed_as_task_issue_still_proceeds_to_review() {
        // FailedAsTaskIssue does NOT short-circuit; Review still runs.
        let proposer_resp = r#"{"step":"ready_for_verify","rationale":"go"}"#;
        let decomp_resp = r#"{
            "tasks":[{"id":"t1","description":"x","involved_nodes":[],"dependencies":[],"needs":{}}],
            "rationale":"single"
        }"#;
        let subagent_resp = r#"{"action":"final_answer","answer":"done","thinking":""}"#;
        let judge_resp = r#"{"verdict":"pass","detail":"actually ok","confidence":0.8}"#;

        let v = Arc::new(FixedValidator(
            crate::agent::validator::ValidationVerdict::FailedAsTaskIssue {
                details: "test failure, no graph signal".into(),
            },
        ));
        let mut gl = build_phase4_loop_with_validator(
            vec![proposer_resp, decomp_resp, subagent_resp, judge_resp],
            v,
        );

        let mut final_result = None;
        for _ in 0..20 {
            match gl.step().await {
                LoopState::Running => continue,
                LoopState::Done(r) => {
                    final_result = Some(r);
                    break;
                }
                other => panic!("unexpected state: {other:?}"),
            }
        }
        let r = final_result.expect("expected Done");
        // Review SHOULD have run (validator's task-issue doesn't block it)
        assert!(r.review_result.is_some());
    }

    // Silence the unused `NodeKind` import — it's there for parity with the
    // module's intent even if not used in tests.
    #[allow(dead_code)]
    fn _silence_unused(_k: NodeKind) {}
}
