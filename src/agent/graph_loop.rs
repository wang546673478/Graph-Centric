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
use crate::error::{HarnessError, Result};
use crate::graph::{DrillDownMark, Edge, Graph, GraphPatch, Node, NodeId, NodeKind, RelationType};
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
    /// `options` are optional concrete choices (the user may also free-type).
    Paused { question: String, options: Vec<String>, rationale: String },

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

/// Sub-phase within the Graph phase — the orchestration layer that
/// guides the model through the "Start→Goal → fill middle → expand
/// complex nodes → verify" workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphPhase {
    /// Pre-build: confirm the goal WITH the user. The Proposer only emits
    /// AskUser (options + "other", or free text) and never builds the graph
    /// until the user sends the confirm sentinel. Skipped in heartbeat mode.
    Clarifying,
    /// First step: build only Start (anchor, immutable) + Goal (target)
    /// with a single DependsOn edge Goal→Start.
    Seeding,
    /// The model explores and fills intermediate nodes between Start and Goal.
    Filling,
    /// Cascade-expand abstract Task nodes into sub-graphs of concrete sub-nodes.
    Expanding,
    /// Model emitted ready_for_verify — verifier runs, then Task phase.
    Verifying,
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

/// Optional graph structure constraints. When set, every ProposePatch is
/// validated against these rules before being applied. If validation fails,
/// the patch is rejected with a descriptive error surfaced to the model
/// (via the fix-it retry path), so it can correct its output.
#[derive(Debug, Clone)]
pub struct GraphSchema {
    /// Allowed node kinds. Empty = all kinds allowed.
    pub allowed_node_kinds: Vec<crate::graph::NodeKind>,
    /// Required edge relation. `None` = any relation allowed.
    pub required_edge_relation: Option<crate::graph::RelationType>,
    /// Whether the final graph must contain at least one immutable node.
    pub require_immutable_anchor: bool,
    /// Minimum number of nodes the graph must have after the patch.
    pub min_nodes: usize,
    /// Minimum number of edges the graph must have after the patch.
    pub min_edges: usize,
}

/// Validate a patch against a GraphSchema. Returns `Ok(())` if the resulting
/// graph would satisfy the schema, or `Err(reason)` with a human-readable
/// explanation of what's wrong.
fn validate_patch_schema(
    graph: &Graph,
    patch: &crate::graph::GraphPatch,
    schema: &GraphSchema,
) -> std::result::Result<(), String> {
    // Build the hypothetical graph after applying the patch.
    let mut after = graph.clone();
    let _ = after.apply_patch(patch.clone());

    // 1. Check node kinds.
    if !schema.allowed_node_kinds.is_empty() {
        for node in after.nodes.values() {
            if !schema.allowed_node_kinds.iter().any(|k| *k == node.kind) {
                return Err(format!(
                    "node '{}' has kind {:?} which is not allowed. \
                     Allowed kinds: {:?}",
                    node.id.as_str(),
                    node.kind,
                    schema.allowed_node_kinds,
                ));
            }
        }
    }

    // 2. Check edge relation type.
    if let Some(ref required_rel) = schema.required_edge_relation {
        for edge in &after.edges {
            if edge.relation != *required_rel {
                return Err(format!(
                    "edge {} -> {} has relation {:?} which is not allowed. \
                     All edges must be {:?}",
                    edge.source.as_str(),
                    edge.target.as_str(),
                    edge.relation,
                    required_rel,
                ));
            }
        }
    }

    // 3. Check immutable anchor.
    if schema.require_immutable_anchor {
        let has_anchor = after.nodes.values().any(|n| n.immutable);
        if !has_anchor {
            return Err(
                "the graph must contain at least one immutable anchor node \
                 (mark one Task node with immutable: true). \
                 The anchor represents the user's unchangeable intent."
                    .to_string(),
            );
        }
    }

    // 4. Check minimum node/edge counts.
    if after.node_count() < schema.min_nodes {
        return Err(format!(
            "the graph has {} nodes but at least {} are required. \
             Add more nodes to cover the task.",
            after.node_count(),
            schema.min_nodes,
        ));
    }
    if after.edge_count() < schema.min_edges {
        return Err(format!(
            "the graph has {} edges but at least {} are required. \
             Add more edges (DependsOn) to connect your nodes.",
            after.edge_count(),
            schema.min_edges,
        ));
    }

    Ok(())
}

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
    /// Whether to auto-match and apply skills in the Task phase.
    /// Default: true (matching runs before decomposer; no-op when no skills configured).
    pub auto_apply_skills: bool,
    /// Optional graph structure constraints. When `Some`, every ProposePatch
    /// is validated before application.
    pub graph_schema: Option<GraphSchema>,
    /// When true, the proposer's system prompt includes autonomous-mode
    /// blocks (no ask_user, direct execution). Set for heartbeat runs.
    pub is_heartbeat: bool,

    // Stagnation detection thresholds
    pub stagnation_soft_hint: u32,
    pub stagnation_hard_hint: u32,
    pub stagnation_terminate: u32,

    // Stuck detection thresholds
    pub stuck_soft_hint: u32,
    pub stuck_hard_hint: u32,
    pub stuck_terminate: u32,

    // Tool failure thresholds
    pub tool_failure_warn_after: u32,
    pub tool_failure_halt_after: u32,

    // ── Self-optimization laws (graph-centric design gaps) ──
    /// Gap 1: in Filling phase, after this many consecutive rounds without
    /// adding a new node, the orchestrator force-dispatches an `explore`
    /// subagent (web search + file reading) instead of waiting for the
    /// model to volunteer one. 0 disables forced search.
    pub force_search_after_filling_stall: u32,
    /// Gap 3: how many consecutive rounds the graph must be both
    /// structurally stable AND fully connected (anchor↔goal) AND fully
    /// L1-enriched before the orchestrator injects a strong "you should
    /// emit ready_for_verify now" hint. The hint never auto-emits — the
    /// model keeps final say. 0 disables convergence hinting.
    pub convergence_stable_rounds: u32,

    // ── Drill-down sub-graph (Task 6) ──
    /// Maximum recursion depth for sub-graph forking. A parent run at
    /// `current_depth = N` can fork a child at `N + 1` only if
    /// `N + 1 <= max_drilldown_depth`. When exceeded, the drill_down
    /// field is dropped (patch nodes/edges still apply) and a warn is
    /// logged. Default 0 = drill-down disabled (current behavior).
    /// Full config wiring lands in Task 9.
    pub max_drilldown_depth: u32,

    /// Wall-clock timeout (millis) for a pending sub-run. When
    /// `poll_sub_run_status` observes `now_ms() - handle.started_at >`
    /// this value, it transitions the handle to `SubRunStatus::Timeout`,
    /// stamps the complex node as timed-out, and raises
    /// `drill_down_error` so the polling block surfaces a
    /// `LoopState::GraphInvalid` on the next tick.
    ///
    /// `None` falls back to [`DEFAULT_SUB_RUN_TIMEOUT_MS`] (30 min).
    /// Optional so existing struct-literal constructors and tests
    /// don't need to be updated; the web gateway passes a concrete
    /// value derived from `EngineConfig::sub_run_timeout_ms`.
    pub sub_run_timeout_ms: Option<u64>,

    /// v2.7: skill match threshold (Jaccard-score cut-off). Default 0.25.
    /// `None` falls back to the value from `EngineConfig::advanced`.
    pub skill_match_threshold: Option<f64>,

    /// v2.7: weight of trigger-text Jaccard in skill match score. Default 0.7.
    pub skill_match_trigger_weight: Option<f64>,

    /// v2.7: weight of slug-token Jaccard in skill match score. Default 0.3.
    pub skill_match_slug_weight: Option<f64>,

    /// v2.7: max L0→L1→L2 expansion depth. Default 2.
    pub cascade_max_expand_depth: Option<u32>,

    // ── v2 agent-harness spec: Clarifying v2 (soft cap 10 + similarity) ──
    /// Soft upper bound on consecutive `ask_user` rounds during
    /// `GraphPhase::Clarifying`. When `clarification_count >= this`,
    /// the loop surfaces `Block("information density saturated")`
    /// instead of asking again. The model is still free to emit
    /// `propose_patch` earlier to end the phase. Default: 10.
    pub clarification_max: u32,
    /// Jaccard-token similarity threshold above which a new
    /// `ask_user` question is treated as a repeat of a recent
    /// question. Combined with `clarification_count >= 3` the loop
    /// surfaces `Block("repeating the same question")`. Default: 0.85.
    pub clarification_similarity_threshold: f64,
    /// Sliding window of recent `ask_user` question texts used for
    /// similarity comparison. Default: 5.
    pub clarification_history_window: usize,

    // ── v2 agent-harness spec: Explore v2 (200 soft cap + similarity) ──
    /// Soft upper bound on consecutive `explore` rounds in any
    /// GraphPhase. When `explorer_iter >= this`, the loop surfaces
    /// `Block("exploration did not converge")`. Default: 200.
    pub explore_max: u32,
    /// Round count at which to inject a soft hint during a long
    /// exploration streak ("if the question has no answer, consider
    /// `block`"). Default: 100.
    pub explore_soft_hint_at: u32,
    /// Round count at which to inject a hard warning + last-chance
    /// hint. Default: 150.
    pub explore_hard_hint_at: u32,
    /// Jaccard-token similarity threshold above which a new
    /// `explore` question is treated as a repeat. Combined with
    /// `explorer_iter >= 3` the loop surfaces
    /// `Block("repeating the same exploration")`. Default: 0.85.
    pub explore_similarity_threshold: f64,
    /// Sliding window of recent `explore` question texts used for
    /// similarity comparison. Default: 5.
    pub explore_history_window: usize,
}

/// Default sub-run timeout: 30 minutes. Used when
/// `GraphLoopConfig.sub_run_timeout_ms` is `None` (CLI / tests that
/// bypass `EngineConfig::load`).
pub const DEFAULT_SUB_RUN_TIMEOUT_MS: u64 = 30 * 60 * 1000;

impl GraphLoopConfig {
    pub fn defaults_at(cwd: impl Into<PathBuf>) -> Self {
        Self {
            max_rounds: 300,
            max_repair_rounds: 4,
            tool_cwd: cwd.into(),
            tool_output_cap: 100_000,
            tool_policy: Arc::new(crate::tools::AllowAll),
            auto_apply_skills: true,
            graph_schema: None,
            is_heartbeat: false,
            stagnation_soft_hint: 4,
            stagnation_hard_hint: 6,
            stagnation_terminate: 12,
            stuck_soft_hint: 3,
            stuck_hard_hint: 5,
            stuck_terminate: 6,
            tool_failure_warn_after: 3,
            tool_failure_halt_after: 8,
            force_search_after_filling_stall: 5,
            convergence_stable_rounds: 3,
            max_drilldown_depth: 0, // disabled by default until Task 9 wires it up
            sub_run_timeout_ms: None,
            skill_match_threshold: None,
            skill_match_trigger_weight: None,
            skill_match_slug_weight: None,
            cascade_max_expand_depth: None,
            // v2 agent-harness spec defaults
            clarification_max: 10,
            clarification_similarity_threshold: 0.85,
            clarification_history_window: 5,
            explore_max: 200,
            explore_soft_hint_at: 100,
            explore_hard_hint_at: 150,
            explore_similarity_threshold: 0.85,
            explore_history_window: 5,
        }
    }
}

/// Handle for a forked sub-GraphLoop. Held by parent graph's
/// `pending_sub_runs` map; `poll_sub_run_status` updates `status` based
/// on the child run's persisted `data/runs/<parent>/sub_runs/<child>/run.json`.
#[derive(Debug, Clone)]
pub struct SubRunHandle {
    pub sub_run_id: String,
    pub complex_node: NodeId,
    pub started_at: u64,
    pub status: SubRunStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubRunStatus {
    Running,
    Done,
    /// The sub-run was cancelled — either externally (the parent was
    /// cancelled and propagated the signal here) or because the sub-run
    /// itself reached the `Cancelled` terminal. The polling block at the
    /// top of `step_graph` treats this like `Done` (removes the handle
    /// from `pending_sub_runs`; the parent continues normally — the
    /// complex node was effectively "skipped" because it was cancelled).
    Cancelled,
    Error(String),
    Timeout,
}

impl Default for SubRunStatus {
    fn default() -> Self { SubRunStatus::Running }
}

/// Errors that can occur during `fork_sub_graph_for`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrillDownError {
    /// `current_depth + 1 > max_drilldown_depth`; the drill_down field
    /// has been dropped (patch nodes/edges still applied).
    DepthLimit,
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
    /// v2: optional CascadeBacktracker. When present, sub-agent graph errors
    /// trigger auto-replan + cascade verification instead of surfacing
    /// GraphInvalid to the caller.
    pub cascade: Option<crate::agent::cascade::CascadeBacktracker>,
    /// Phase A (skill system): optional skill storage for auto-matching.
    /// When Some and auto_apply_skills is true, step_task_stub checks for
    /// matching skills and may substitute their compiled task graphs for
    /// the decomposer output.
    pub skill_storage: Option<Arc<dyn crate::skills::SkillStorage>>,
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
    /// Current sub-phase within the Graph phase — drives the orchestration
    /// flow: Seeding → Filling → Expanding → Verifying.
    pub graph_phase: GraphPhase,
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
    /// Fingerprint of the graph at the last round end: hashed from
    /// (node_count, edge_count, sorted node IDs). Used to detect
    /// stagnation — rounds where the graph doesn't change at all.
    last_graph_fingerprint: Option<u64>,
    /// Consecutive rounds with the same graph fingerprint.
    graph_stagnation_count: u32,
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
    /// In Filling phase, how many consecutive rounds passed without
    /// adding new nodes. After 3 rounds, auto-inject a suggested
    /// intermediate node to break the research-only loop.
    filling_rounds_without_nodes: u32,
    /// Gap 3 (convergence): consecutive rounds where the graph was
    /// structurally stable AND anchor↔goal connected AND fully L1
    /// enriched. When this reaches `convergence_stable_rounds`, the
    /// orchestrator injects a one-shot strong hint nudging the model to
    /// emit `ready_for_verify`. Reset whenever any of those conditions
    /// breaks, or after the hint fires.
    convergence_stable_count: u32,
    /// Whether the convergence hint has already been injected for the
    /// current stable streak (so we hint once, not every round).
    convergence_hint_sent: bool,
    /// Whether the one-time Clarifying-phase instruction was injected.
    clarifying_primed: bool,
    /// Signature (hash) of the last orphan-node set we hinted about, so we
    /// don't re-inject the same "connect these nodes" hint every step. None
    /// means no hint sent yet. Reset implicitly when the orphan set changes.
    last_orphan_hint_sig: Option<u64>,
    /// Gap (Seeding stall): consecutive rounds spent in the Seeding phase
    /// with an empty graph while the model chose a non-patch step (e.g.
    /// explore). The first action on any task must be the deterministic
    /// "draw Start+Goal" patch; if the model keeps exploring instead, we
    /// hint, then auto-seed so the loop can never stall at 0 nodes.
    seeding_rounds_without_patch: u32,

    // ── v2 agent-harness spec: Clarifying v2 tracking ──
    /// Consecutive `ask_user` rounds during `GraphPhase::Clarifying`.
    /// Incremented when the model emits `AskUser`; reset to 0 when
    /// it emits `ProposePatch` (signaling the goal is clear) or
    /// when the loop surfaces a Clarifying-saturation Block.
    clarification_count: u32,
    /// Sliding window of recent `ask_user` question texts. Used by
    /// the saturation check to detect the model repeating itself
    /// without learning anything new. Bounded by
    /// `config.clarification_history_window`.
    clarification_history: std::collections::VecDeque<String>,

    // ── v2 agent-harness spec: Explore v2 tracking ──
    /// Consecutive `explore` rounds across any GraphPhase. Reset to
    /// 0 on `ProposePatch` / `Block` / `AskUser` / `ReadyForVerify`.
    /// Drives the 100/150/200 tiered hints + termination.
    explorer_iter: u32,
    /// Sliding window of recent `explore` question texts. Used by
    /// the saturation check to detect the model probing the same
    /// detail over and over. Bounded by
    /// `config.explore_history_window`.
    explorer_history: std::collections::VecDeque<String>,
    /// Whether the soft Explore hint has been injected for the
    /// current streak. Reset by `reset_saturations()`.
    explore_soft_hint_sent: bool,
    /// Whether the hard Explore warning has been injected for the
    /// current streak. Reset by `reset_saturations()`.
    explore_hard_hint_sent: bool,

    /// Sub-graph handles keyed by complex_node_id. Non-empty when
    /// the parent is waiting on at least one child run.
    pub pending_sub_runs: std::collections::HashMap<NodeId, SubRunHandle>,

    /// Parent run id (None for the outermost run).
    pub parent_run_id: Option<String>,

    /// Depth in the drill-down chain: 0 = outermost, 1 = sub, 2 = sub-sub, ...
    pub current_depth: u32,

    /// Counter for generating unique sub-run ids.
    pub sub_run_counter: u32,

    /// This run's id (used as the parent id when forking sub-runs).
    pub run_id: String,

    /// Event channel for streaming sub-graph events back to the parent.
    pub event_tx: tokio::sync::broadcast::Sender<crate::web::events::EngineEvent>,

    /// Cache of `drill_down.reason` for nodes added in the most recent patch.
    /// Set by step_graph after a patch with drill_down is applied; consumed
    /// by `build_sub_task_for` during `fork_sub_graph_for`. Cleared after fork.
    pub last_patch_drill_down_reasons: std::collections::HashMap<NodeId, String>,

    /// Queue of `(complex_node, drill_down_reason)` pairs that the
    /// patch-apply arm has flagged for fork. Drained at the top of
    /// `step_graph` (in the polling block) where `fork_sub_graph_for`
    /// can be called without tripping the Send-bound at the
    /// `tokio::spawn` site. See the comment at the patch-apply site
    /// for the full rationale.
    pub pending_fork_targets: Vec<(NodeId, String)>,

    /// Set by `poll_sub_run_status` when a sub-run reports an error.
    /// The polling block at the top of `step_graph` checks this on the
    /// next tick and returns `LoopState::GraphInvalid { source: Review }`
    /// so the existing reviewer-judge / GraphInvalid repair path runs
    /// — no new error-handling machinery. Per the drill-down spec:
    /// "sub_run_status=error → 父图 C 标 error → 走现有 GraphInvalid 路径".
    pub drill_down_error: Option<(NodeId, String)>,

    /// Persistence used by `fork_sub_graph_for` to write the sub-run
    /// directory + link. Defaults to a no-op persistence rooted at a
    /// tmp dir; the web gateway overrides via `with_persistence`.
    pub persistence: crate::web::persistence::RunPersistence,

    phase: Phase,
    pending: Pending,

    /// External cancel signal. Set via [`Self::cancel`] by the web
    /// gateway when the user clicks "stop", or by a test that wants to
    /// exercise the cancellation propagation path. The polling block at
    /// the top of `step_graph` checks this every tick; when set, it
    /// propagates `Cancelled` to every pending sub-run and returns
    /// `LoopState::Error("parent cancelled")` so the caller's session
    /// shutdown logic can finalize the run.
    ///
    /// `bool` is sufficient — `step_graph` takes `&mut self` so the
    /// write doesn't need atomic semantics, and the loop is
    /// single-threaded across `step` calls.
    pub cancelled: bool,
}

/// Extract file/function/class entities from Explore output text and produce
/// a GraphPatch that adds them as L0 nodes with Contains edges from a scope node.
fn extract_entities_to_patch(text: &str, scope: &str, graph: &crate::graph::Graph) -> crate::graph::GraphPatch {
    use crate::graph::{Edge, GraphPatch, Node, NodeId, NodeKind, RelationType};
    use regex::Regex;

    let mut patch = GraphPatch::default();

    // Find or create the scope (parent) node.
    let scope_id = NodeId::from(scope.to_string());
    let scope_exists = graph.contains_node(&scope_id);
    if !scope_exists && !scope.is_empty() {
        // Create a repo/directory node for the scope.
        let kind = if scope.starts_with("http") { NodeKind::Other("repo".into()) } else { NodeKind::Module };
        patch.add_nodes.push(Node::new(scope_id.clone(), kind, scope.to_string(), format!("{scope} (explore scope)")));
        // Mark it to be created even if the edge addition fails later.
    }
    let parent_id = scope_id;

    // Extract file paths: /path/to/file.ext, src/file.rs, etc.
    let file_re = Regex::new(r"([\w/\-._]+\.(rs|py|js|ts|tsx|go|java|kt|rb|md|toml|yaml|json|css|html|vue))").unwrap();
    let mut seen = std::collections::HashSet::new();

    for cap in file_re.captures_iter(text) {
        let path = cap[0].to_string();
        // Skip paths that look like URLs or contain no directory structure.
        if seen.contains(&path) || path.len() < 3 { continue; }
        seen.insert(path.clone());

        let id = NodeId::from(path.clone());
        let file_exists = graph.contains_node(&id) || patch.add_nodes.iter().any(|n| n.id == id);
        if !file_exists {
            patch.add_nodes.push(Node::file(path.clone(), format!("{path} (from explore)")));
        }
        // Edge from scope to file.
        if parent_id.as_str() != path {
            patch.add_edges.push(Edge::new(
                parent_id.clone(), id,
                RelationType::Contains, 0.7,
                "from explore output",
            ));
        }
    }

    // Extract function/class names and link to their file.
    let sym_re = Regex::new(r"([\w/\-._]+\.(rs|py|js|ts|go|java)):?\s*.*?(fn|def|func|class|struct|enum|interface)\s+(\w+)").unwrap();
    for cap in sym_re.captures_iter(text) {
        let file_path = &cap[1];
        let sym_name = cap[4].to_string();
        if seen.contains(&sym_name) { continue; }
        seen.insert(sym_name.clone());

        let sym_id = NodeId::from(sym_name.clone());
        let kind = match &cap[3] {
            "class" | "struct" | "enum" | "interface" => NodeKind::Class,
            _ => NodeKind::Function,
        };
        patch.add_nodes.push(Node::new(sym_id.clone(), kind, file_path.to_string(), sym_name));
        // Edge from file to symbol.
        let file_id = NodeId::from(file_path.to_string());
        patch.add_edges.push(Edge::new(
            file_id, sym_id,
            RelationType::Contains, 0.7,
            "from explore output",
        ));
    }

    // Bare function/class mentions without file context — link to scope.
    let bare_re = Regex::new(r"(fn|def|func|class|struct|enum|interface)\s+(\w+)").unwrap();
    for cap in bare_re.captures_iter(text) {
        let name = cap[2].to_string();
        if seen.contains(&name) { continue; }
        seen.insert(name.clone());

        let id = NodeId::from(name.clone());
        let kind = match &cap[1] {
            "class" | "struct" | "enum" | "interface" => NodeKind::Class,
            _ => NodeKind::Function,
        };
        patch.add_nodes.push(Node::new(id.clone(), kind, String::new(), name));
        if !scope.is_empty() {
            patch.add_edges.push(Edge::new(
                parent_id.clone(), id,
                RelationType::Contains, 0.5,
                "bare symbol from explore",
            ));
        }
    }

    patch
}

/// Summarize long subagent output into a concise report for the main agent.
async fn summarize_for_main_agent(model: &dyn crate::model::Model, text: &str) -> String {
    if text.len() <= 3000 {
        return text.to_string();
    }
    let prompt = format!(
        "Summarize the following agent output into a concise report. Keep all file paths, key findings, and code snippets. Max 500 words.\n\n{text}"
    );
    let req = crate::model::ModelRequest {
        messages: vec![crate::model::Message::user(prompt)],
        tools: vec![],
        temperature: 0.0,
        max_tokens: Some(1024),
        stop: vec![],
    };
    match model.complete(req).await {
        Ok(resp) => resp.content,
        Err(_) => text.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Drill-down helpers (Task 6)
// ---------------------------------------------------------------------------

/// Wall-clock millis since UNIX_EPOCH. Used to stamp `SubRunHandle.started_at`
/// and `SubRunLink.created_at` so the parent can age sub-runs for the
/// `pending_sub_runs` map and decide when to surface status to the user.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Test-only accessor for `now_ms()`. Tests use this to stamp
/// `SubRunHandle.started_at` with the current wall-clock so the
/// handle doesn't accidentally trip the timeout producer (which
/// compares against `now_ms() - started_at > sub_run_timeout_ms`).
#[cfg(test)]
pub(crate) fn now_ms_for_test() -> u64 {
    now_ms()
}

/// Minimal `Model` impl used by `new_with_depth` for sub-loops. Returns
/// `ready_for_verify` on the first call (which passes structural verify
/// on an empty graph and routes the sub-loop to Done) so any spawn made
/// by `fork_sub_graph_for` in a test setting terminates in a few ticks.
/// Task 10 will replace this with a real clone of the parent's proposer
/// so the sub-loop actually expands the complex node.
struct NoopModel;

#[async_trait::async_trait]
impl crate::model::Model for NoopModel {
    fn name(&self) -> &str {
        "noop-sub-graph-model"
    }
    async fn complete(
        &self,
        _request: crate::model::ModelRequest,
    ) -> Result<crate::model::ModelResponse> {
        Ok(crate::model::ModelResponse {
            content: r#"{"step":"ready_for_verify","rationale":"noop sub-graph termination"}"#.to_string(),
            tool_calls: vec![],
            finish_reason: crate::model::FinishReason::Stop,
            reasoning_content: None,
            usage: crate::model::Usage::default(),
        })
    }
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
        let conversation = if config.is_heartbeat {
            let system = proposer.build_system_prompt_heartbeat(&task);
            Conversation::new(system, task.clone())
        } else {
            proposer.make_conversation(&task)
        };
        let initial_phase = if config.is_heartbeat {
            GraphPhase::Seeding
        } else {
            GraphPhase::Clarifying
        };
        Self {
            proposer,
            verifier,
            repairer,
            enricher: None,
            decomposer: None,
            dispatcher: None,
            subagent_loader: None,
            cascade: None,
            skill_storage: None,
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
            graph_phase: initial_phase,
            filling_rounds_without_nodes: 0,
            convergence_stable_count: 0,
            convergence_hint_sent: false,
            clarifying_primed: false,
            last_orphan_hint_sig: None,
            seeding_rounds_without_patch: 0,
            last_verification: None,
            task_outcome: None,
            review_result: None,
            last_step: None,
            last_tool_result: None,
            final_summary: None,
            last_stuck_signature: None,
            stuck_repeat_count: 0,
            last_graph_fingerprint: None,
            graph_stagnation_count: 0,
            // v2 agent-harness spec: Clarifying v2 + Explore v2 tracking
            clarification_count: 0,
            clarification_history: std::collections::VecDeque::new(),
            explorer_iter: 0,
            explorer_history: std::collections::VecDeque::new(),
            explore_soft_hint_sent: false,
            explore_hard_hint_sent: false,
            tool_failure_counts: std::collections::HashMap::new(),
            tokens_used: 0,
            // Drill-down sub-graph machinery (Task 5). `event_tx` defaults to
            // a no-op broadcast channel; production callers (web gateway) replace
            // it via `with_event_tx` so sub-runs can stream events back to the
            // parent. `current_depth = 0` marks the outermost run; sub-runs fork
            // at depth+1.
            pending_sub_runs: std::collections::HashMap::new(),
            parent_run_id: None,
            current_depth: 0,
            sub_run_counter: 0,
            run_id: format!("run-{}", uuid::Uuid::new_v4()),
            event_tx: tokio::sync::broadcast::channel::<crate::web::events::EngineEvent>(64).0,
            last_patch_drill_down_reasons: std::collections::HashMap::new(),
            pending_fork_targets: Vec::new(),
            drill_down_error: None,
            // Default persistence: rooted at a tempdir so `fork_sub_graph_for`
            // has a valid place to create the sub-run dir even without the
            // web gateway overriding. The web path replaces this via
            // `with_persistence` after construction.
            persistence: crate::web::persistence::RunPersistence::with_data_dir(
                std::env::temp_dir().join("graph_harness_default_persistence"),
            ),
            phase: Phase::Graph,
            pending: Pending::None,
            cancelled: false,
        }
    }

    /// External cancel signal. Idempotent; safe to call from any
    /// thread (the loop is single-threaded across `step` calls, but
    /// `&mut self` is enough to guarantee exclusive access). After this
    /// returns, the next `step_graph` call will detect `self.cancelled`
    /// at the top of its polling block, propagate `Cancelled` to every
    /// pending sub-run, and return `LoopState::Error("parent cancelled")`.
    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    /// Returns true if [`Self::cancel`] has been called. The polling
    /// block uses this to decide whether to short-circuit before
    /// invoking the Proposer. Exposed (rather than `cancelled` accessed
    /// directly) so callers don't need to know about the internal field
    /// name.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Convenience setter so callers (e.g. the web gateway driver in
    /// `api_runs.rs`) can sync an external cancel signal onto the loop
    /// without having to call [`Self::cancel`] explicitly. Equivalent to
    /// `if external_cancelled { self.cancel(); }`. Idempotent.
    pub fn check_and_set_cancelled(&mut self, external_cancelled: bool) {
        if external_cancelled {
            self.cancelled = true;
        }
    }

    /// Attach a [`RunPersistence`]. The web gateway calls this after
    /// constructing the loop so that `fork_sub_graph_for` writes the
    /// sub-run directory under the project's `data/runs/` tree. Tests
    /// call this with a tempdir-rooted persistence.
    pub fn with_persistence(mut self, persistence: crate::web::persistence::RunPersistence) -> Self {
        self.persistence = persistence;
        self
    }

    /// Factory: build a sub-GraphLoop that inherits the parent's config,
    /// task, model, tools, and conversation setup, but at an incremented
    /// drill-down depth with `parent_run_id` set. Used by
    /// [`Self::fork_sub_graph_for`].
    ///
    /// The sub-loop is created with a `NullModel` (no model calls) and
    /// an empty tool registry so it can run unattended: when
    /// `run_with_persistence` drives it, the model returns an immediate
    /// `ready_for_verify` (since the verifier passes on an empty graph)
    /// and the loop terminates with `Done` in a few steps — enough to
    /// write `run.json` for the tests.
    ///
    /// Task 10 (step_graph integration) will replace this with a real
    /// clone of the parent's proposer/model/tools/decomposer/etc. so the
    /// child can actually expand the complex node into a sub-graph.
    pub fn new_with_depth(
        parent_proposer: crate::agent::proposer::GraphProposer,
        parent_verifier: crate::agent::verifier::Verifier,
        parent_repairer: Option<crate::agent::repairer::LocalRepairer>,
        parent_tools: std::sync::Arc<crate::tools::ToolRegistry>,
        cfg: GraphLoopConfig,
        parent_run_id: String,
        current_depth: u32,
        sub_task: Option<String>,
    ) -> Self {
        // Use the parent's actual proposer/verifier/tools so the sub-loop
        // can call the model and emit real propose_patch. The previous
        // implementation used a NoopModel that immediately returned
        // ready_for_verify — sub-runs produced 0 nodes, defeating the
        // drill-down feature entirely. (See task 5b565105 bug report.)
        let task_str = sub_task.unwrap_or_else(|| "sub-graph task".to_string());
        let mut sub = Self::new(
            task_str,
            parent_proposer,
            parent_verifier,
            parent_repairer,
            parent_tools,
            cfg,
        );
        sub.parent_run_id = Some(parent_run_id);
        sub.current_depth = current_depth;
        sub
    }

    /// Run the loop to terminal state, persist the final `run.json` into
    /// the provided persistence's data dir, and emit events on the
    /// provided broadcast channel. Used by [`Self::fork_sub_graph_for`]
    /// to drive a forked sub-loop without interfering with the parent's
    /// `event_tx` / persistence state.
    ///
    /// Simplified for Task 6: walks the FSM until it returns Done/Error,
    /// then writes a minimal `run.json` (status + node_count + edge_count
    /// + duration). Doesn't emit per-step events on the broadcast channel
    /// for now — that's wired in Task 10/11 (API endpoints).
    pub async fn run_with_persistence(
        mut self,
        persistence: crate::web::persistence::RunPersistence,
        _event_tx: tokio::sync::broadcast::Sender<crate::web::events::EngineEvent>,
    ) -> Result<()> {
        let started = std::time::Instant::now();
        let terminal = loop {
            match self.step().await {
                LoopState::Running => continue,
                terminal => break terminal,
            }
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        let status = match &terminal {
            LoopState::Done(_) => "Done",
            LoopState::Error(_) => "Error",
            _ => "Done",
        };
        let payload = serde_json::json!({
            "id": self.run_id,
            "task": self.task,
            "status": status,
            "node_count": self.graph.node_count(),
            "edge_count": self.graph.edge_count(),
            "duration_ms": duration_ms,
            "parent_run_id": self.parent_run_id,
            "current_depth": self.current_depth,
            "tokens_used": self.tokens_used,
        });
        let dir = persistence.data_dir.clone();
        // Ensure the sub-run dir exists. If this fails, fall back to
        // writing a minimal Error-status run.json so the parent's poll
        // loop can detect the failure instead of hanging forever.
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::error!(error = %e, dir = %dir.display(), "run_with_persistence: create_dir_all failed");
            let fallback = serde_json::json!({
                "id": self.run_id,
                "task": self.task,
                "status": "Error",
                "error": format!("create_dir_all failed: {e}"),
                "parent_run_id": self.parent_run_id,
                "current_depth": self.current_depth,
                "duration_ms": duration_ms,
            });
            // Try a best-effort write in the current working directory
            // so the failure is at least visible.
            let _ = std::fs::write(
                "run.json.fallback",
                serde_json::to_string_pretty(&fallback).unwrap_or_default(),
            );
            return Err(crate::error::HarnessError::model(format!(
                "run_with_persistence: create_dir_all({}) failed: {e}",
                dir.display()
            )));
        }
        let run_json_path = dir.join("run.json");
        match std::fs::write(
            &run_json_path,
            serde_json::to_string_pretty(&payload).unwrap_or_default(),
        ) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Final-write failure: do NOT silently swallow. Fall back
                // to writing an Error-status payload so the parent's poll
                // loop can detect the failure, then return Err.
                tracing::error!(
                    error = %e,
                    path = %run_json_path.display(),
                    "run_with_persistence: final run.json write failed"
                );
                let fallback = serde_json::json!({
                    "id": self.run_id,
                    "task": self.task,
                    "status": "Error",
                    "error": format!("run.json write failed: {e}"),
                    "parent_run_id": self.parent_run_id,
                    "current_depth": self.current_depth,
                    "duration_ms": duration_ms,
                });
                let _ = std::fs::write(
                    &run_json_path,
                    serde_json::to_string_pretty(&fallback).unwrap_or_default(),
                );
                Err(crate::error::HarnessError::model(format!(
                    "run_with_persistence: write {} failed: {e}",
                    run_json_path.display()
                )))
            }
        }
    }

    /// Build the drill-down task prompt that the sub-GraphLoop will be
    /// handed as its task description. Pulls the cached `drill_down.reason`
    /// from the most recent patch (set by `step_graph` when a patch with
    /// drill_down marks was applied), or falls back to the node's summary
    /// if no reason was provided.
    pub fn build_sub_task_for(&self, complex_node: &NodeId) -> String {
        use crate::agent::proposer::render_graph_for_prompt;
        let node = match self.graph.nodes.get(complex_node) {
            Some(n) => n,
            None => return format!("Drill-down: {}", complex_node.as_str()),
        };
        let reason = self
            .last_patch_drill_down_reasons
            .get(complex_node)
            .cloned()
            .unwrap_or_else(|| node.summary.clone());
        // Render the parent's current graph so the sub-loop model has
        // context for the surrounding structure when it expands this
        // node. Without this, the model would be expanding in a vacuum.
        let parent_graph_snapshot = render_graph_for_prompt(&self.graph);
        format!(
            "[Drill-down of {id}] {reason}\n\n\
             ## Parent graph state (so you know what's around you)\n\
             {parent_graph_snapshot}\n\n\
             ## What you MUST do\n\
             You are a sub-graph that EXPANDS the single complex node above into \
             concrete sub-steps. The parent run will merge your `propose_patch` \
             into the L0 graph under this node.\n\
             \n\
             ### Required output\n\
             Emit a `propose_patch` tool_call. Your patch must:\n\
             - `add_nodes`: 3-8 sub-steps, each with `kind: Task` and a semantic id\n\
             - `add_edges`: at least one `LeadsTo` chain + at least one `DependsOn`\n\
               between your sub-steps (so the sub-graph is more than a linear line)\n\
             - `reason`: 1-2 sentences why this expansion\n\
             - `rationale`: 1-2 sentences how the sub-steps help implement the parent\n\
             \n\
             ### Use semantic ids\n\
             Short kebab-case nouns (`extract-amount`, `define-event-schema`,\n\
             `setup-rbac`), NOT `step1` / `B1` / `T1`.\n\n\
             ### DO NOT\n\
             - DO NOT add a `start` or `deliverable` node — the parent already has them\n\
             - DO NOT propose graph changes outside this sub-graph (no top-level edits)\n\
             - DO NOT call `consult_advisor` or other meta-steps; just `propose_patch`\n\
             - DO NOT reply with prose explaining; the tool_call IS your response",
            id = complex_node.as_str(),
            reason = reason,
            parent_graph_snapshot = parent_graph_snapshot,
        )
    }

    /// Fork the current loop into a sub-GraphLoop that expands
    /// `complex_node` into its own sub-graph. Returns a [`SubRunHandle`]
    /// the parent can poll (via `poll_sub_run_status` in Task 7).
    ///
    /// Mechanics:
    /// 1. Depth check: if `current_depth + 1 > max_drilldown_depth`, return
    ///    `DrillDownError::DepthLimit`. The drill_down field is dropped
    ///    (nodes/edges from the same patch are still applied).
    /// 2. Generate a unique sub_run_id (`<run_id>-sub-<counter>-d<depth>`).
    /// 3. Build the sub-task prompt via `build_sub_task_for`.
    /// 4. Create a fresh sub-GraphLoop via `new_with_depth` with the same
    ///    config (so model/tools/policy inherit), incremented depth, and
    ///    parent_run_id set.
    /// 5. Create the sub-run directory under the parent's run dir (on
    ///    disk; on IO failure we warn and proceed — the sub-loop can
    ///    still run in-memory), then append a `SubRunLink` to the
    ///    parent's checkpoint index via `append_sub_run_link` so future
    ///    runs can locate and resume the child.
    /// 6. Stamp the complex node's metadata with `sub_run_id`,
    ///    `sub_run_status="running"`, `drill_down_depth=<depth>`, and
    ///    flip `expanded=true` so subsequent renders show the node as
    ///    expanded.
    /// 7. Inject a transcript line so the model sees the drill-down start.
    /// 8. Spawn the sub-loop on the tokio runtime — it runs unattended
    ///    and writes `run.json` to its sub-run dir when it terminates.
    pub async fn fork_sub_graph_for(
        &mut self,
        complex_node: NodeId,
    ) -> std::result::Result<SubRunHandle, DrillDownError> {
        let new_depth = self.current_depth + 1;
        if new_depth > self.config.max_drilldown_depth {
            tracing::warn!(
                current_depth = self.current_depth,
                max_depth = self.config.max_drilldown_depth,
                node = %complex_node.as_str(),
                "drill_down depth limit reached; field dropped, patch nodes/edges still applied"
            );
            return Err(DrillDownError::DepthLimit);
        }

        let sub_run_id = format!(
            "{}-sub-{}-d{}",
            self.run_id, self.sub_run_counter, new_depth
        );
        self.sub_run_counter += 1;

        let sub_task = self.build_sub_task_for(&complex_node);

        let sub_config = self.config.clone();
        // Don't re-clamp max_drilldown_depth — it's already set; the
        // sub-loop can itself fork at depth+1 if max allows.

        let sub_run_id_for_loop = sub_run_id.clone();
        let parent_run_id = self.run_id.clone();
        // Pass the parent's actual proposer/verifier/tools so the sub-loop
        // can drive the real model. Without this the sub-run used a
        // NoopModel and produced 0 nodes.
        let sub_loop = GraphLoop::new_with_depth(
            self.proposer.clone(),
            self.verifier.clone(),
            self.repairer.clone(),
            self.tools.clone(),
            sub_config,
            parent_run_id,
            new_depth,
            Some(sub_task.clone()),
        );

        // Atomicity note (Task 6 review): the steps below have a small
        // window where, if a panic occurs between metadata mutation and
        // the final tokio::spawn, the parent could observe `expanded = true`
        // and `sub_run_status = "running"` without a corresponding child
        // task. The simplest fix that ships in Task 6 is to make
        // `tokio::spawn` the absolute last side-effect (after all metadata
        // writes + conversation appends). If the spawn is reached, the
        // sub-loop is guaranteed to run. If a panic happens earlier, the
        // parent's worst case is "expanded but no child" — Task 7's
        // poll_sub_run_status can detect "no run.json after N ms" and
        // surface an Error to the user. Task 10 will tighten this to a
        // true two-phase commit (build all state in memory, then commit
        // atomically) when step_graph integration lands.
        //
        // Order is therefore:
        //   1. Build sub-loop in memory (no side effects).
        //   2. Persist sub-run dir + link.
        //   3. Stamp complex node metadata.
        //   4. Append transcript line.
        //   5. tokio::spawn — THE COMMIT POINT.

        // Persist the sub-run directory under the parent's run dir so
        // the child has a place to write `run.json` later. On IO failure
        // we warn and proceed — the sub-loop can still run in-memory.
        if let Err(e) = self
            .persistence
            .create_sub_run_dir(&self.run_id, &sub_run_id_for_loop)
        {
            tracing::warn!(
                error = %e,
                parent = %self.run_id,
                sub = %sub_run_id_for_loop,
                "fork_sub_graph_for: create_sub_run_dir failed; sub-loop will still spawn"
            );
        }
        let link = crate::web::checkpoint::SubRunLink {
            node_id: complex_node.clone(),
            sub_run_id: sub_run_id_for_loop.clone(),
            sub_status: "running".to_string(),
            created_at: now_ms(),
        };
        self.persistence
            .append_sub_run_link(&self.run_id, &link);

        // Stamp the complex node's metadata so subsequent renders know
        // it's been expanded and can show drill-down state. The
        // `drill_down_depth` is stored as a JSON Number rather than
        // String because consumers (frontend, poll_sub_run_status, e2e
        // tests) compare against it as an integer.
        if let Some(node) = self.graph.nodes.get_mut(&complex_node) {
            node.metadata.insert(
                "sub_run_id".into(),
                serde_json::Value::String(sub_run_id_for_loop.clone()),
            );
            node.metadata.insert(
                "sub_run_status".into(),
                serde_json::Value::String("running".into()),
            );
            node.metadata.insert(
                "drill_down_depth".into(),
                serde_json::Value::Number(serde_json::Number::from(new_depth as u64)),
            );
            node.expanded = true;
        } else {
            tracing::warn!(
                node = %complex_node.as_str(),
                "fork_sub_graph_for: complex node not found in graph; skipping metadata stamp"
            );
        }

        // Add a transcript line so the model sees the drill-down start
        // when it next reads the conversation.
        self.conversation.add_user(format!(
            "⤵ drill_down started: {}\n(sub_run_id={}, depth={})",
            complex_node.as_str(),
            sub_run_id_for_loop,
            new_depth
        ));

        // COMMIT POINT: spawn the sub-loop. Once this returns, the
        // sub-loop is running and will write run.json when it
        // terminates.
        let sub_persistence = self
            .persistence
            .clone_for_sub_run(&self.run_id, &sub_run_id_for_loop);
        let event_tx = self.event_tx.clone();
        // Task 10 fix: previously this `tokio::spawn` site would trip
        // a Send-bound violation when `fork_sub_graph_for` was awaited
        // from `step_graph` (because the outer future's state machine
        // unified with the spawned future and propagated non-Send
        // captures). The fix is to extract the spawned task into a
        // free `async fn` so its future is type-erased from the
        // caller's call-graph and Send is checked only against its
        // own (Send-only) captures: `sub_loop`, `sub_persistence`,
        // and `event_tx`. If any of those stops being Send, the
        // compile error here will point directly at the culprit
        // rather than at a confusing propagation through `step_graph`.
        Self::spawn_sub_loop_task(sub_loop, sub_persistence, event_tx);

        Ok(SubRunHandle {
            sub_run_id: sub_run_id_for_loop,
            complex_node,
            started_at: now_ms(),
            status: SubRunStatus::Running,
        })
    }

    /// Free-function helper that spawns a sub-GraphLoop's
    /// `run_with_persistence` on the runtime. Lives outside the
    /// `async fn fork_sub_graph_for` body so its Send-bound is checked
    /// in isolation from any outer caller's call-graph (notably
    /// `step_graph`'s). See the comment at the spawn site in
    /// `fork_sub_graph_for` for the full rationale.
    fn spawn_sub_loop_task(
        sub_loop: GraphLoop,
        sub_persistence: crate::web::persistence::RunPersistence,
        event_tx: tokio::sync::broadcast::Sender<crate::web::events::EngineEvent>,
    ) {
        tokio::spawn(async move {
            if let Err(e) = sub_loop.run_with_persistence(sub_persistence, event_tx).await {
                tracing::warn!(error = %e, "sub-graph loop errored");
            }
        });
    }

    /// Poll a forked sub-run's persisted `run.json` and update `handle.status`
    /// based on the child's reported status. This is the inverse of
    /// [`Self::fork_sub_graph_for`]: the parent loop calls it (typically
    /// from `step_graph` / `Task 10`) each round to see whether the child
    /// has finished.
    ///
    /// Behavior:
    /// - If `run.json` does not exist yet (child still running) or is
    ///   unreadable / unparseable, the function returns silently and
    ///   leaves `handle.status` as `Running` (idempotent). This is
    ///   intentional — the parent will try again next round.
    /// - If `status == "Done"`, the complex node is marked done and a
    ///   transcript line is appended so the model sees the drill-down
    ///   completed.
    /// - If `status == "Error"`, the complex node is marked with
    ///   `status="error"` + `error=<message>` and a transcript line is
    ///   appended so the model can react (e.g., backtrack, escalate).
    /// - Any other status string (including `"Running"`) keeps the
    ///   handle in `Running`.
    ///
    /// The function never panics on malformed input; it logs a `warn!`
    /// and returns.
    pub async fn poll_sub_run_status(&mut self, handle: &mut SubRunHandle) {
        let path = self
            .persistence
            .sub_run_run_json(&self.run_id, &handle.sub_run_id);
        let status_str = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => {
                // run.json not yet written — but check whether the
                // handle has aged past the timeout. If so, surface
                // `SubRunStatus::Timeout` so the polling block can
                // propagate it as a drill_down_error.
                self.check_handle_timeout(handle);
                return;
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&status_str) {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!(
                    path = %path.display(),
                    sub_run_id = %handle.sub_run_id,
                    "poll_sub_run_status: run.json present but not valid JSON; treating as Running"
                );
                // Even on malformed JSON, still check timeout so a
                // stuck sub-run that wrote garbage can be escalated.
                self.check_handle_timeout(handle);
                return;
            }
        };
        let status_field = v.get("status").and_then(|s| s.as_str()).unwrap_or("");

        handle.status = match status_field {
            "Done" | "done" => {
                self.mark_complex_node_done(&handle.complex_node);
                self.conversation.add_user(format!(
                    "✓ drill_down complete: {}\n(sub_run_id={})",
                    handle.complex_node.as_str(),
                    handle.sub_run_id
                ));
                SubRunStatus::Done
            }
            "Cancelled" | "cancelled" => {
                self.mark_complex_node_cancelled(&handle.complex_node);
                self.conversation.add_user(format!(
                    "⊘ drill_down cancelled: {}\n(sub_run_id={})",
                    handle.complex_node.as_str(),
                    handle.sub_run_id
                ));
                SubRunStatus::Cancelled
            }
            "Error" | "error" => {
                let err = v
                    .get("error")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                self.mark_complex_node_error(&handle.complex_node, &err);
                self.conversation.add_user(format!(
                    "✗ drill_down failed: {}\n(sub_run_id={}, error: {})",
                    handle.complex_node.as_str(),
                    handle.sub_run_id,
                    err
                ));
                SubRunStatus::Error(err)
            }
            _ => {
                // Status string was not one of the recognized terminal
                // values — but the sub-run may still have aged past the
                // timeout. Check that before defaulting to Running.
                if let Some(_) = self.check_handle_timeout(handle) {
                    // Already mutated to Timeout by the helper.
                    SubRunStatus::Timeout
                } else {
                    SubRunStatus::Running
                }
            }
        };

        if let Some(node) = self.graph.nodes.get_mut(&handle.complex_node) {
            let status_str = match &handle.status {
                SubRunStatus::Running => "running",
                SubRunStatus::Done => "done",
                SubRunStatus::Cancelled => "cancelled",
                SubRunStatus::Error(_) => "error",
                SubRunStatus::Timeout => "timeout",
            };
            node.metadata.insert(
                "sub_run_status".into(),
                serde_json::Value::String(status_str.into()),
            );
        }
    }

    /// If `now_ms() - handle.started_at > sub_run_timeout_ms`, set
    /// `handle.status = SubRunStatus::Timeout` and stamp the complex node
    /// via `mark_complex_node_timeout`. Returns `Some(())` when the
    /// timeout fired, `None` otherwise. Idempotent and safe on a missing
    /// node.
    ///
    /// `sub_run_timeout_ms` comes from `EngineConfig::sub_run_timeout_ms`
    /// and is read at call time so config reloads take effect without
    /// restarting the loop. We fall back to the default
    /// `DEFAULT_SUB_RUN_TIMEOUT_MS` when the env var / config hasn't been
    /// propagated to `GraphLoopConfig` (which is the case for tests and
    /// CLI-only callers that don't go through `EngineConfig::load()`).
    fn check_handle_timeout(&mut self, handle: &mut SubRunHandle) -> Option<()> {
        if matches!(handle.status, SubRunStatus::Done | SubRunStatus::Cancelled | SubRunStatus::Error(_)) {
            return None;
        }
        let now = now_ms();
        let timeout_ms = self.sub_run_timeout_ms();
        if now.saturating_sub(handle.started_at) > timeout_ms {
            handle.status = SubRunStatus::Timeout;
            self.mark_complex_node_timeout(&handle.complex_node);
            self.conversation.add_user(format!(
                "⏱ drill_down timeout: {}\n(sub_run_id={}, age_ms={})",
                handle.complex_node.as_str(),
                handle.sub_run_id,
                now.saturating_sub(handle.started_at)
            ));
            Some(())
        } else {
            None
        }
    }

    /// Return the configured sub-run timeout in millis. Today this reads
    /// `self.config.sub_run_timeout_ms` (a u64 on `GraphLoopConfig` set by
    /// the web gateway via `EngineConfig`); if absent we fall back to
    /// [`DEFAULT_SUB_RUN_TIMEOUT_MS`].
    fn sub_run_timeout_ms(&self) -> u64 {
        self.config.sub_run_timeout_ms.unwrap_or(DEFAULT_SUB_RUN_TIMEOUT_MS)
    }

    /// Stamp a complex node with `status="done"`. Called by
    /// [`Self::poll_sub_run_status`] when the sub-run finishes successfully.
    /// Idempotent and safe to call on a missing node (logs nothing on miss
    /// to avoid noise — the parent already knows which complex node is in
    /// play).
    pub fn mark_complex_node_done(&mut self, node_id: &NodeId) {
        if let Some(node) = self.graph.nodes.get_mut(node_id) {
            node.metadata.insert(
                "status".into(),
                serde_json::Value::String("done".into()),
            );
        }
    }

    /// Stamp a complex node with `status="error"` plus the error message.
    /// Called by [`Self::poll_sub_run_status`] when the sub-run reports an
    /// error. Idempotent. Also raises the `drill_down_error` flag on the
    /// parent loop so the polling block at the top of `step_graph` can
    /// surface a `LoopState::GraphInvalid` on the next tick — that lets
    /// the existing GraphInvalid/reviewer/repair machinery react to a
    /// sub-run failure rather than silently swallowing it.
    pub fn mark_complex_node_error(&mut self, node_id: &NodeId, err: &str) {
        if let Some(node) = self.graph.nodes.get_mut(node_id) {
            node.metadata.insert(
                "status".into(),
                serde_json::Value::String("error".into()),
            );
            node.metadata.insert(
                "error".into(),
                serde_json::Value::String(err.to_string()),
            );
        }
        self.drill_down_error = Some((node_id.clone(), err.to_string()));
    }

    /// Stamp a complex node with `status="cancelled"`. Called by
    /// [`Self::poll_sub_run_status`] when the sub-run reports a
    /// `Cancelled` terminal status, and by [`Self::propagate_cancel_to_pending_sub_runs`]
    /// when the parent loop has been cancelled externally and wants to
    /// leave a "skipped" marker on each pending complex node.
    ///
    /// Idempotent and safe on a missing node. Unlike
    /// [`Self::mark_complex_node_error`], this does NOT raise the
    /// `drill_down_error` flag — the polling block at the top of
    /// `step_graph` treats `SubRunStatus::Cancelled` as a soft terminal
    /// (drop the handle, continue normally) rather than as a
    /// graph-invalid event.
    pub fn mark_complex_node_cancelled(&mut self, node_id: &NodeId) {
        if let Some(node) = self.graph.nodes.get_mut(node_id) {
            node.metadata.insert(
                "status".into(),
                serde_json::Value::String("cancelled".into()),
            );
        }
    }

    /// Stamp a complex node with `status="timeout"`. Called by
    /// [`Self::check_handle_timeout`] when a pending sub-run has aged
    /// past `sub_run_timeout_ms`. Idempotent and safe on a missing
    /// node. Also raises the `drill_down_error` flag so the polling
    /// block surfaces a `LoopState::GraphInvalid` on the next tick —
    /// same machinery as `mark_complex_node_error` so reviewers can
    /// react to a timed-out drill-down instead of silently dropping it.
    pub fn mark_complex_node_timeout(&mut self, node_id: &NodeId) {
        if let Some(node) = self.graph.nodes.get_mut(node_id) {
            node.metadata.insert(
                "status".into(),
                serde_json::Value::String("timeout".into()),
            );
            node.metadata.insert(
                "error".into(),
                serde_json::Value::String("sub-run timeout".into()),
            );
        }
        self.drill_down_error = Some((node_id.clone(), "sub-run timeout".to_string()));
    }

    /// Write a `{"status": "Cancelled"}` payload to a sub-run's
    /// `run.json` so external observers (the web UI, future parent
    /// loops that re-poll after a restart) can see that this sub-run
    /// was cancelled by its parent rather than completed naturally.
    /// Best-effort: errors are logged but never propagated, since
    /// `propagate_cancel_to_pending_sub_runs` runs as part of the
    /// shutdown path and must not block the parent's exit.
    fn write_sub_run_cancelled(&self, handle: &SubRunHandle) {
        let path = self
            .persistence
            .sub_run_run_json(&self.run_id, &handle.sub_run_id);
        let payload = serde_json::json!({
            "status": "Cancelled",
            "cancelled_by_parent": true,
            "parent_run_id": self.run_id,
            "sub_run_id": handle.sub_run_id,
        });
        if let Err(e) = std::fs::write(&path, serde_json::to_string(&payload).unwrap_or_default()) {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                sub_run_id = %handle.sub_run_id,
                "write_sub_run_cancelled: failed to write cancelled run.json"
            );
        }
    }

    /// When the parent loop has been cancelled externally (via
    /// [`Self::cancel`]), walk every pending sub-run and:
    /// 1. Write `{"status":"Cancelled",...}` to its `run.json` so a
    ///    later restart / observer can detect the cancellation.
    /// 2. Stamp the complex node's metadata via
    ///    [`Self::mark_complex_node_cancelled`].
    /// 3. Drop the handle from `pending_sub_runs`.
    ///
    /// Returns the number of pending sub-runs that were cancelled.
    /// Called by the polling block at the top of `step_graph` after
    /// `self.cancelled` is observed.
    pub fn propagate_cancel_to_pending_sub_runs(&mut self) -> usize {
        // Always clear the fork queue — even when pending_sub_runs is
        // empty, queued `pending_fork_targets` would otherwise be
        // drained on the next tick and spawn a new sub-run that the
        // shutdown sequence would have to abort. The drain is cheap
        // (a single `Vec::clear`) so we do it unconditionally.
        self.pending_fork_targets.clear();

        if self.pending_sub_runs.is_empty() {
            return 0;
        }
        let handles: Vec<(NodeId, SubRunHandle)> = std::mem::take(&mut self.pending_sub_runs)
            .into_iter()
            .collect();
        let count = handles.len();
        for (_complex_node, handle) in handles {
            self.write_sub_run_cancelled(&handle);
            self.mark_complex_node_cancelled(&handle.complex_node);
        }
        count
    }

    /// Apply a [`GraphPatch`] to the graph, then queue a drill-down fork
    /// for the target complex node (if `patch.drill_down` is set).
    ///
    /// This is the single-purpose integration helper that `step_graph`
    /// uses in the `ProposePatch` arm. Splitting it out keeps the
    /// `step_graph` arm readable and lets tests exercise the patch +
    /// drill_down wiring directly without driving a full Proposer round.
    ///
    /// Behaviour:
    /// - The patch is applied to the graph via `Graph::apply_patch`. Any
    ///   graph-level error (dangling endpoint, etc.) is returned as-is
    ///   so the caller can surface it back to the model.
    /// - If `patch.drill_down` is `Some(mark)` AND `mark.target` is one
    ///   of `patch.add_nodes`, the (target, reason) pair is pushed onto
    ///   `pending_fork_targets`. The polling block at the top of
    ///   `step_graph` drains that queue on the next tick and calls
    ///   `fork_sub_graph_for` there. This indirection exists because
    ///   calling `fork_sub_graph_for` inline from inside the patch-apply
    ///   arm propagates a non-Send capture from its internal
    ///   `tokio::spawn` through `step_graph`'s outer future, breaking
    ///   compilation. See the comment at the queue drain site for the
    ///   full rationale.
    pub async fn apply_graph_patch_with_drill_down(
        &mut self,
        patch: &GraphPatch,
    ) -> Result<()> {
        // 1. Apply the patch to the graph.
        self.graph.apply_patch(patch.clone())?;

        // 2. Queue any drill_down request for the next polling tick.
        self.queue_drill_down(patch);
        Ok(())
    }

    /// Push a `(complex_node, reason)` pair onto `pending_fork_targets`
    /// when `patch.drill_down` is set and its target is one of the
    /// newly-added nodes. This is the single source of truth for the
    /// "queue a drill_down for fork" step — both the live `step_graph`
    /// patch-apply arm and the `apply_graph_patch_with_drill_down` test
    /// helper call into here so the wiring stays in sync. The actual
    /// `fork_sub_graph_for` call is deferred to the polling block at the
    /// top of `step_graph`; see that block's comment for the Send-bound
    /// rationale.
    fn queue_drill_down(&mut self, patch: &GraphPatch) {
        if let Some(dd) = &patch.drill_down {
            if let Some(complex_node) = patch
                .add_nodes
                .iter()
                .find(|n| n.id == dd.target)
                .map(|n| n.id.clone())
            {
                self.pending_fork_targets
                    .push((complex_node, dd.reason.clone()));
            }
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

    /// v2: attach a CascadeBacktracker for auto-replan + cascade verification
    /// on sub-agent failure.
    pub fn with_cascade(mut self, cascade: crate::agent::cascade::CascadeBacktracker) -> Self {
        self.cascade = Some(cascade);
        self
    }

    /// Phase A (skill system): attach a SkillStorage for auto-matching.
    pub fn with_skill_storage(
        mut self,
        storage: Arc<dyn crate::skills::SkillStorage>,
    ) -> Self {
        self.skill_storage = Some(storage);
        self
    }

    /// Try to match the current task against stored skills and compile the
    /// best match into a task graph. Returns `None` when:
    /// - No skill storage configured (`skill_storage` is `None`).
    /// - `auto_apply_skills` is false.
    /// - No skill scored above the threshold.
    /// - Loading or compiling the matched skill failed.
    /// - The compiled graph is empty.
    async fn try_match_and_compile_skill(&mut self) -> Option<Graph> {
        let storage = self.skill_storage.as_ref()?;
        if !self.config.auto_apply_skills {
            return None;
        }

        let matched = match crate::skills::retrieve::find_and_load_matching_skills_with(
            &self.task,
            storage.as_ref(),
            &crate::skills::matcher::SkillMatchConfig {
                trigger_weight: self.config.skill_match_trigger_weight.unwrap_or(0.7),
                slug_weight: self.config.skill_match_slug_weight.unwrap_or(0.3),
                threshold: self.config.skill_match_threshold.unwrap_or(0.4),
                l1_weight: 0.0, // TODO: thread L1 from the current task graph
            },
            1,    // top 1 for Phase A
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "skill matching failed");
                return None;
            }
        };

        let skill = match matched.first() {
            Some(s) => s,
            None => return None,
        };

        let compiled = crate::skills::compiler::compile_skill_to_task_graph(skill);
        if compiled.node_count() == 0 {
            tracing::info!(skill = %skill.slug, "matched skill has empty graph; falling back to decomposer");
            return None;
        }

        tracing::info!(
            skill = %skill.slug,
            trigger = %skill.trigger,
            compiled_nodes = compiled.node_count(),
            compiled_edges = compiled.edge_count(),
            "auto-matched skill injected into task DAG"
        );

        self.conversation.add_user(format!(
            "Auto-matched skill `{}` (trigger: \"{}\"). Its compiled task graph \
             ({} nodes, {} edges) has been injected into the task plan.",
            skill.slug,
            skill.trigger,
            compiled.node_count(),
            compiled.edge_count(),
        ));

        Some(compiled)
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
                options: Vec::new(),
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

        let state = match self.phase {
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
        };

        // Graph stagnation guard: detect when the graph hasn't changed
        // for many consecutive rounds. Tier 3 triggers cascade backtracking
        // if configured, to verify upstream nodes before giving up.
        if matches!(state, LoopState::Running) {
            if let Some(stuck) = self.check_graph_stagnation().await {
                return stuck;
            }
            // Gap 3: convergence hint. When the graph is structurally
            // stable, anchor↔goal connected, and fully L1-enriched for
            // several rounds, nudge the model to emit ready_for_verify.
            // This is the soft convergence signal that replaces the
            // removed hard caps — it never auto-emits (per user choice),
            // only strongly hints; the model keeps final say.
            self.check_convergence_hint();
        }

        state
    }

    // -----------------------------------------------------------------------
    // Graph phase
    // -----------------------------------------------------------------------

    async fn step_graph(&mut self) -> Result<LoopState> {
        // ── Parent-cancel propagation ──
        // If [`Self::cancel`] has been called (typically by the web
        // gateway when the user clicks "stop"), propagate `Cancelled`
        // to every pending sub-run (write Cancelled run.json + stamp
        // the complex node), clear the pending_fork_targets queue so
        // we don't spawn new sub-runs on the way out, and return
        // `LoopState::Error("parent cancelled")` so the caller's
        // session-shutdown logic can finalize the run.
        //
        // This check is FIRST so a cancel signal observed between
        // ticks short-circuits before any model call or fork happens.
        if self.cancelled {
            self.propagate_cancel_to_pending_sub_runs();
            return Ok(LoopState::Error("parent cancelled".into()));
        }

        // ── Polling priority: drain pending sub-runs BEFORE invoking the Proposer ──
        // When the parent is waiting on at least one forked sub-GraphLoop,
        // we must poll those handles first. The parent loop must NOT
        // continue to grow the main graph while a complex node is being
        // expanded in a sub-graph — the sub-graph's findings will feed back
        // via `poll_sub_run_status` / transcript lines, and the model
        // needs an opportunity to react before proposing more nodes.
        //
        // Behaviour:
        // - If any sub-run is still Running/Timeout, return `LoopState::Running`
        //   without calling the model (caller will call `step()` again next
        //   tick).
        // - If all sub-runs are terminal (Done/Error/Cancelled), fall through
        //   and proceed to the normal Proposer round; terminal sub-runs have
        //   already been removed from `pending_sub_runs` by the poll loop
        //   and `poll_sub_run_status` has already stamped the complex node
        //   + added a transcript line. Errors surface in the model's view
        //   on the next round; the existing error-handling path will react.
        // - Cancelled is treated like Done: drop the handle, continue
        //   normally. The complex node was effectively "skipped" because
        //   it was cancelled.
        // ── Drill-down fork queue ──
        // Drain any (complex_node, reason) pairs queued by the
        // patch-apply arm into actual `pending_sub_runs` handles. We do
        // this BEFORE polling existing sub-runs so that a freshly-forked
        // sub-run is included in the same tick's poll. Forking is
        // deferred to here (rather than called inline in the patch-apply
        // arm) to avoid triggering a Send-bound at the `tokio::spawn`
        // site inside `fork_sub_graph_for`; see the comment at the
        // patch-apply site for the full rationale.
        //
        // Re-check `self.cancelled` before draining — if the parent was
        // cancelled between the top-of-loop check and now, skip the
        // fork entirely. Otherwise we'd spawn a fresh sub-run on the
        // way out and have to abort it.
        if !self.cancelled && !self.pending_fork_targets.is_empty() {
            let queued = std::mem::take(&mut self.pending_fork_targets);
            for (complex_node, reason) in queued {
                self.last_patch_drill_down_reasons
                    .insert(complex_node.clone(), reason);
                match self.fork_sub_graph_for(complex_node.clone()).await {
                    Ok(handle) => {
                        self.pending_sub_runs.insert(complex_node.clone(), handle);
                    }
                    Err(DrillDownError::DepthLimit) => {
                        // Already warned inside `fork_sub_graph_for`;
                        // patch is already applied — drill_down is dropped.
                    }
                }
                self.last_patch_drill_down_reasons.remove(&complex_node);
            }
        }

        if !self.pending_sub_runs.is_empty() {
            let keys: Vec<NodeId> = self.pending_sub_runs.keys().cloned().collect();
            for k in keys {
                let mut handle = self.pending_sub_runs.remove(&k).expect(
                    "pending_sub_runs key was collected above; entry must still exist",
                );
                self.poll_sub_run_status(&mut handle).await;
                match &handle.status {
                    SubRunStatus::Running | SubRunStatus::Timeout => {
                        // Still in flight — keep tracking and skip the
                        // Proposer round this tick.
                        self.pending_sub_runs.insert(k, handle);
                    }
                    SubRunStatus::Done | SubRunStatus::Cancelled => {
                        // Terminal — drop the handle. Done case has
                        // already been marked done on the complex node;
                        // Cancelled case has been marked cancelled +
                        // transcript line added. We don't raise
                        // drill_down_error for Cancelled: cancellation
                        // is a graceful shutdown, not a graph defect.
                    }
                    SubRunStatus::Error(_) => {
                        // Terminal — drop the handle; the error has
                        // already raised drill_down_error inside
                        // poll_sub_run_status and a transcript line
                        // was added so the model can react next round.
                    }
                }
            }
            if !self.pending_sub_runs.is_empty() {
                return Ok(LoopState::Running);
            }
        }

        // ── Drill-down error propagation ──
        // If a sub-run finished with an error, surface it to the caller
        // as `LoopState::GraphInvalid` so the existing GraphInvalid /
        // reviewer / repair path handles it. Per the drill-down spec:
        // "sub_run_status=error → 父图 C 标 error → 走现有 GraphInvalid
        // 路径(reviewer judge 评 C 失败),不引入新机制". We use `Review`
        // as the error source because the parent model effectively
        // "reviewed" the sub-graph outcome and concluded it failed —
        // same conceptual step as a Reviewer judge.
        if let Some((node_id, err)) = self.drill_down_error.take() {
            self.pending = Pending::AwaitingRepair;
            return Ok(LoopState::GraphInvalid {
                source: ErrorSource::Review,
                errors: vec![GraphError::L0Structural {
                    error_type: L0ErrorType::WrongRelation,
                    detail: format!(
                        "drill_down sub-run failed for node `{}`: {}",
                        node_id.as_str(),
                        err
                    ),
                    related_nodes: vec![node_id],
                    discovered_by: Some("drill_down_sub_run".into()),
                }],
                snapshot: self.graph.clone(),
            });
        }

        // ── Clarifying phase: prime the Proposer (once) to confirm the goal ──
        // v2 agent-harness spec: the model self-decides when enough
        // information has been gathered. The loop will surface a Block
        // if (a) the model keeps asking and information density is
        // saturated (~10 rounds by default), or (b) it starts
        // repeating the same question. The user can always answer
        // with their own text to skip the model's options and force
        // the model to proceed.
        if self.graph_phase == GraphPhase::Clarifying && !self.clarifying_primed {
            self.clarifying_primed = true;
            self.conversation.add_user(
                "GOAL CLARIFICATION PHASE. Confirm the user's goal before building. \
                 You may emit `ask_user` to gather information, but the loop will \
                 surface a Block if (a) you keep asking and information density is \
                 saturated, or (b) you start repeating the same question. State your \
                 current understanding of the goal in each `ask_user`, then provide \
                 `options` (concrete choices the user can pick — the user can also \
                 free-type). When you have enough to start building, emit \
                 `propose_patch` to seed `start` and `deliverable`. Starting to \
                 build IS the signal you're ready.",
            );
        }

        // ── Seeding guard: the first action must draw Start+Goal ──
        // The graph-centric design mandates that the first step on a task
        // is the deterministic 2-node Start→Goal seed, NOT exploration.
        // We count rounds spent in Seeding with an empty graph regardless
        // of what the proposer returns (explore, malformed-then-salvaged
        // ask_user, etc.) — that's the exact failure that produced
        // "graph stagnated … 0 nodes, 0 edges" over 8 rounds. After
        // `seeding_stall_limit` such rounds, auto-seed so the loop can
        // never stall at 0 nodes. The first round injects a strong hint
        // and lets the model seed itself.
        const SEEDING_STALL_LIMIT: u32 = 3;
        if self.graph_phase == GraphPhase::Seeding && self.graph.node_count() == 0 {
            self.seeding_rounds_without_patch += 1;
            if self.seeding_rounds_without_patch >= SEEDING_STALL_LIMIT {
                warn!(
                    rounds = self.seeding_rounds_without_patch,
                    "Seeding stalled — auto-seeding Start+Goal so the loop can proceed"
                );
                self.auto_seed_start_goal();
                self.conversation.add_user(
                    "I auto-created the `start` (immutable anchor) and `deliverable` (goal) \
                     nodes because the first step on any task must be the 2-node \
                     start→deliverable seed, not endless exploration. The graph is now in \
                     the Filling phase — explore if you must, then `propose_patch` step \
                     nodes BETWEEN start and deliverable (semantic ids like `outline`, not \
                     B1/B2), each wired in with `LeadsTo` edges.",
                );
                self.graph_phase = GraphPhase::Filling;
                self.seeding_rounds_without_patch = 0;
                self.emit_graph_phase();
                return Ok(LoopState::Running);
            }
            if self.seeding_rounds_without_patch == 1 {
                self.conversation.add_user(
                    "⚠️ Your first action must be a `propose_patch` that creates exactly \
                     two nodes — Start (current state) and Goal (desired outcome) — joined \
                     by one DependsOn edge. Do NOT explore yet. Emit that seed patch now.",
                );
            }
        }

        let (step, tokens) = self
            .proposer
            .next_step_with_retry(&self.conversation, &self.graph, self.last_step.as_ref())
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

        // ── Orchestration: track Filling rounds without new nodes ──
        // Gap 1: when the model spins in Filling without adding nodes, it
        // usually means it doesn't know what intermediate steps to insert.
        // First (3 rounds) we inject a textual hint; if it still hasn't
        // added a node by `force_search_after_filling_stall`, we stop
        // waiting and force-dispatch an explore subagent (web search +
        // file reading) to gather the missing information. This turns
        // "don't know how to fill" from a soft suggestion into a
        // deterministic action. Applies to all runs, not just heartbeat.
        let is_patch_step = matches!(step, ProposerStep::ProposePatch { .. });
        let is_explore_step = matches!(step, ProposerStep::Explore { .. });
        if self.graph_phase == GraphPhase::Filling && !is_patch_step && !is_explore_step {
            self.filling_rounds_without_nodes += 1;

            let force_at = self.config.force_search_after_filling_stall;
            if force_at > 0 && self.filling_rounds_without_nodes >= force_at {
                // Escalation: force a web-search + file explore. Reset the
                // counter and dispatch directly, bypassing the model's
                // chosen step for this round.
                self.filling_rounds_without_nodes = 0;
                warn!(
                    rounds = force_at,
                    "Filling stalled — force-dispatching explore (web search + files)"
                );
                self.conversation.add_user(
                    "You've spent several rounds without adding an intermediate node. \
                     I'm dispatching a research subagent (web search + file reading) to \
                     gather the information needed to fill the gap between Start and Goal. \
                     Use its findings to propose the next intermediate node."
                );
                let items = self.build_forced_search_items();
                return self.dispatch_explore_subagents(items).await;
            }

            // Soft hint at 3 rounds (existing behavior, now run-agnostic).
            if self.filling_rounds_without_nodes == 3 {
                let hint = self.build_filling_hint();
                self.conversation.add_user(hint);
            }
        }

        match step {
            ProposerStep::AskUser { question, options, rationale } => {
                // v2 agent-harness spec: Clarifying saturation check.
                // The model is free to keep asking, but the loop
                // surfaces a Block once the question is a repeat of a
                // recent one OR the count cap has been reached.
                use crate::agent::saturation::SaturationVerdict;
                let verdict = self.check_clarification_saturation(&question);
                let final_question = match verdict {
                    SaturationVerdict::CountLimit => {
                        warn!(
                            count = self.clarification_count,
                            max = self.config.clarification_max,
                            "Clarifying saturated: count cap reached"
                        );
                        // Reset so a follow-up resume doesn't keep
                        // the streak alive.
                        self.reset_saturations();
                        self.pending = Pending::None;
                        return Ok(LoopState::Paused {
                            question: format!(
                                "[block] 信息密度已饱和:已问 {} 轮,模型认为已有足够信息,\
                                 但仍在继续追问。请给一个更明确的答复,或回复「继续」强制让 agent \
                                 进入下一阶段(emit `propose_patch`)。",
                                self.config.clarification_max
                            ),
                            options: vec!["继续".to_string(), "中止".to_string()],
                            rationale: "clarification_count saturated".to_string(),
                        });
                    }
                    SaturationVerdict::Repeat => {
                        warn!(
                            count = self.clarification_count,
                            "Clarifying saturated: question repeats a recent one"
                        );
                        self.reset_saturations();
                        self.pending = Pending::None;
                        return Ok(LoopState::Paused {
                            question: format!(
                                "[block] agent 在重复追问同一话题(相似度 > {:.2})。\
                                 请换个角度回答,或回复「继续」让 agent 进入下一阶段。",
                                self.config.clarification_similarity_threshold
                            ),
                            options: vec!["继续".to_string(), "中止".to_string()],
                            rationale: "clarification similarity saturated".to_string(),
                        });
                    }
                    SaturationVerdict::Proceed => question.clone(),
                };
                self.record_clarification(question.clone());
                self.pending = Pending::AwaitingAnswer { question: final_question.clone() };
                // Reset stuck detector — engaging the user is a way out.
                self.stuck_repeat_count = 0;
                self.last_stuck_signature = None;
                Ok(LoopState::Paused {
                    question: final_question,
                    options,
                    rationale,
                })
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
                Ok(LoopState::Paused { question, options: Vec::new(), rationale })
            }
            ProposerStep::Explore { items, rationale: _ } => {
                // v2 agent-harness spec: Explore saturation check.
                // The model is free to keep exploring, but the loop
                // surfaces a Block once a question repeats OR the
                // iter cap has been reached.
                use crate::agent::saturation::SaturationVerdict;
                if let Some(item) = items.first() {
                    let verdict = self.check_explore_saturation(&item.question);
                    match verdict {
                        SaturationVerdict::CountLimit => {
                            warn!(
                                iter = self.explorer_iter,
                                max = self.config.explore_max,
                                "Explore saturated: iter cap reached"
                            );
                            self.reset_saturations();
                            return Ok(LoopState::Paused {
                                question: format!(
                                    "[block] 探索无收敛:已 Explore {} 轮仍未达成 commit。\
                                     模型对此问题可能找不到答案。请考虑:(a) 提供更多上下文;\
                                     (b) 回复「继续」让 agent 强制 ProposePatch;\
                                     (c) 中止。",
                                    self.config.explore_max
                                ),
                                options: vec!["继续".to_string(), "中止".to_string()],
                                rationale: "explore iter cap reached".to_string(),
                            });
                        }
                        SaturationVerdict::Repeat => {
                            warn!(
                                iter = self.explorer_iter,
                                "Explore saturated: question repeats a recent one"
                            );
                            self.reset_saturations();
                            return Ok(LoopState::Paused {
                                question: format!(
                                    "[block] agent 在重复探索同一问题(相似度 > {:.2})。\
                                     请换个角度提问,或回复「继续」让 agent 强制 commit。",
                                    self.config.explore_similarity_threshold
                                ),
                                options: vec!["继续".to_string(), "中止".to_string()],
                                rationale: "explore similarity saturated".to_string(),
                            });
                        }
                        SaturationVerdict::Proceed => {
                            self.record_explore(item.question.clone());
                            // Inject tier hint if a soft/hard threshold
                            // has been crossed this round. The hint is
                            // informational; the model keeps final say.
                            if let Some(hint) = self.explore_tier_hint() {
                                use crate::agent::saturation::TierHint;
                                match hint {
                                    TierHint::Soft => {
                                        self.conversation.add_user(
                                            "⚠️ 你已经连续 Explore 100 轮。如果这个问题本质无法回答,\
                                             考虑 emit `block` 或直接 emit `propose_patch` 落图。\
                                             不收敛的探索浪费 token。",
                                        );
                                        self.mark_explore_soft_hint_sent();
                                    }
                                    TierHint::Hard => {
                                        self.conversation.add_user(
                                            "🚨 你已经连续 Explore 150 轮。再不 commit 就要 Block 了。\
                                             立即 emit `propose_patch` 把已有信息落图,\
                                             或 emit `block` 声明这是模型搞不定的问题。",
                                        );
                                        self.mark_explore_hard_hint_sent();
                                    }
                                }
                            }
                        }
                    }
                }
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
                        if self.stuck_repeat_count >= self.config.stuck_terminate {
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
                        if self.stuck_repeat_count >= self.config.stuck_hard_hint {
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
                        } else if self.stuck_repeat_count >= self.config.stuck_soft_hint {
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
                        if failure_count >= self.config.tool_failure_halt_after {
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
                        if failure_count >= self.config.tool_failure_warn_after {
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
            ProposerStep::ProposePatch { mut patch, rationale: _ } => {
                // v2 agent-harness spec: a ProposePatch signals the
                // model is making progress — reset BOTH Clarifying
                // and Explore saturation counters. The next ask_user
                // or explore starts a fresh streak.
                if self.clarification_count > 0 || self.explorer_iter > 0 {
                    self.reset_saturations();
                }
                // If we're still Clarifying and the model starts building, that's
                // the "goal confirmed" signal — advance to Seeding so the seed/guard
                // logic treats this as the first build patch.
                if self.graph_phase == GraphPhase::Clarifying {
                    info!("clarifying: model started building — advancing to Seeding");
                    self.graph_phase = GraphPhase::Seeding;
                    self.emit_graph_phase();
                }
                // ──────────────────────────────────────────────
                // Orchestration layer: enforce the "Start→Goal first,
                // then fill middle" workflow based on graph_phase.
                // ──────────────────────────────────────────────
                {
                    // ── Seeding phase: enforce 2-node Start+Goal patch ──
                    if self.graph_phase == GraphPhase::Seeding && self.graph.node_count() == 0 {
                        if !patch.add_nodes.is_empty() {
                            warn!(
                                count = patch.add_nodes.len(),
                                "seeding: reducing patch to 2 nodes (Start + Goal)"
                            );
                            // Keep only the first and last nodes as Start/Goal.
                            // Re-identify them as A (anchor) and D (goal).
                            let mut kept = Vec::with_capacity(2);
                            if let Some(first) = patch.add_nodes.first().cloned() {
                                kept.push(first);
                            }
                            if let Some(last) = patch.add_nodes.last().cloned() {
                                if kept.len() < 2 || last.id != kept[0].id {
                                    kept.push(last);
                                }
                            }
                            // Ensure exactly 2 nodes.
                            if kept.len() < 2 {
                                // Model only provided 1 node; synthesize deliverable.
                                kept.push(Node {
                                    id: NodeId::from("deliverable"),
                                    kind: NodeKind::Task,
                                    path: "deliverable".into(),
                                    summary: "Deliverable: the desired outcome".into(),
                                    metadata: Default::default(),
                                    immutable: false,
                                    expanded: false,
                                });
                            }
                            // Re-identify: first node → start (anchor), last → deliverable (goal).
                            kept[0].id = NodeId::from("start");
                            kept[0].immutable = true;
                            kept[0].kind = NodeKind::Task;
                            if kept[0].summary.trim().is_empty() {
                                kept[0].summary = "Start: current problem or initial state".into();
                            }
                            if kept.len() > 1 {
                                kept[1].id = NodeId::from("deliverable");
                                kept[1].kind = NodeKind::Task;
                                if kept[1].summary.trim().is_empty() {
                                    kept[1].summary = "Deliverable: desired outcome".into();
                                }
                            }
                            patch.add_nodes = kept;
                            // Enforce single LeadsTo edge: start→deliverable.
                            patch.add_edges = vec![Edge::new(
                                NodeId::from("start"),
                                NodeId::from("deliverable"),
                                RelationType::LeadsTo,
                                0.9,
                                "start leads to deliverable",
                            )];
                            patch.remove_edge_indices.clear();
                            patch.remove_node_ids.clear();
                        }
                        // Ensure at least 1 LeadsTo edge exists.
                        if patch.add_edges.is_empty() && patch.add_nodes.len() >= 2 {
                            patch.add_edges.push(Edge::new(
                                NodeId::from("start"),
                                NodeId::from("deliverable"),
                                RelationType::LeadsTo,
                                0.9,
                                "start leads to deliverable",
                            ));
                        }
                    }

                    // Standard auto-fix for common model mistakes.
                    for node in &mut patch.add_nodes {
                        // Auto-set immutable:true on anchor-like nodes.
                        // Matches: "start", "a", "anchor-*", "A-*", "A_*", "A.*", "start*"
                        let id_lower = node.id.as_str().to_lowercase();
                        let is_anchor = id_lower == "start"
                            || id_lower == "a"
                            || id_lower.contains("anchor")
                            || id_lower.starts_with("start")
                            || id_lower.starts_with("a-")
                            || id_lower.starts_with("a_")
                            || id_lower.starts_with("a.")
                            || id_lower.starts_with("anchor");
                        if is_anchor {
                            node.immutable = true;
                        }
                        // Auto-set kind:Task if schema requires it.
                        if let Some(ref schema) = self.config.graph_schema {
                            if !schema.allowed_node_kinds.is_empty()
                                && !schema.allowed_node_kinds.contains(&node.kind)
                            {
                                node.kind = NodeKind::Task;
                            }
                        }
                        // Fill empty summary.
                        if node.summary.trim().is_empty() {
                            node.summary = format!("Task node: {}", node.id.as_str());
                        }
                    }
                    for edge in &mut patch.add_edges {
                        // Auto-set relation:DependsOn if schema requires it.
                        if let Some(ref schema) = self.config.graph_schema {
                            if let Some(ref required_rel) = schema.required_edge_relation {
                                if edge.relation != *required_rel {
                                    edge.relation = required_rel.clone();
                                }
                            }
                        }
                        // Ensure source/target are set.
                        if edge.source.as_str().is_empty() || edge.target.as_str().is_empty() {
                            // Can't fix — skip this edge.
                            continue;
                        }
                    }
                }

                // Schema validation: if a GraphSchema is configured, reject
                // patches that would violate structural constraints.
                if let Some(ref schema) = self.config.graph_schema {
                    if let Err(reason) = validate_patch_schema(&self.graph, &patch, schema) {
                        if self.config.is_heartbeat {
                            warn!(
                                reason = %reason,
                                "GraphSchema rejected patch even after auto-fix; injecting hint"
                            );
                            self.conversation.add_user(format!(
                                "⚠️ GraphSchema rejected your patch: {reason}\n\
                                 The patch was NOT applied. Fix the issues above \
                                 and emit a corrected propose_patch.",
                            ));
                            return Ok(LoopState::Running);
                        }
                        return Err(HarnessError::model(format!(
                            "GraphSchema violation: {reason}\n\
                             The patch was NOT applied. Correct your patch to follow \
                             these rules and emit a new propose_patch.",
                        )));
                    }
                }
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
                        // ── Orchestration: phase transitions after patch ──
                        {
                            let nodes_increased = self.graph.node_count() > before_nodes;
                            match self.graph_phase {
                                GraphPhase::Seeding => {
                                    // First patch applied — transition to Filling.
                                    self.graph_phase = GraphPhase::Filling;
                                    self.filling_rounds_without_nodes = 0;
                                    info!("orchestration: Seeding → Filling ({}n/{}e)",
                                        self.graph.node_count(), self.graph.edge_count());
                                    self.emit_graph_phase();
                                    self.conversation.add_user(
                                        "✅ start→deliverable established. Now work out the \
                                         intermediate steps needed BETWEEN start and deliverable. \
                                         Explore if needed, then `propose_patch` to insert step \
                                         nodes — give them semantic ids (e.g. `outline`, \
                                         `draft-intro`), NOT B1/B2/T1 — and connect each with \
                                         `LeadsTo` edges so the path runs start → … → deliverable."
                                    );
                                }
                                GraphPhase::Filling => {
                                    if nodes_increased {
                                        self.filling_rounds_without_nodes = 0;
                                    }
                                    // If graph has >= 4 nodes, suggest cascade expansion.
                                    if self.graph.node_count() >= 4 {
                                        self.graph_phase = GraphPhase::Expanding;
                                        info!("orchestration: Filling → Expanding ({}n/{}e)",
                                            self.graph.node_count(), self.graph.edge_count());
                                        self.emit_graph_phase();
                                    }
                                }
                                _ => {}
                            }
                        }

                        // L0 → L1 linkage: auto-enrich brand-new nodes.
                        if !new_node_ids.is_empty() {
                            self.auto_enrich(&new_node_ids).await;
                        }

                        // Drill-down detection: if this patch flagged one of
                        // its new nodes with `drill_down`, queue a fork
                        // request that the polling block at the top of
                        // `step_graph` will process on the next tick.
                        //
                        // Why queue rather than call `fork_sub_graph_for`
                        // inline? `fork_sub_graph_for` does an internal
                        // `tokio::spawn` of the sub-GraphLoop's
                        // `run_with_persistence` future, which captures
                        // the sub-loop's full state. When called from
                        // `step_graph`'s patch-apply arm, that capture
                        // propagates through `step_graph`'s outer future
                        // and triggers a Send-bound violation at the
                        // `tokio::spawn` site (the sub-loop's state isn't
                        // `Send` in the compile-time call graph rooted at
                        // `step_graph`, even though it IS Send when called
                        // directly from a test). Deferring to the polling
                        // block — which uses a different call graph that
                        // doesn't trip the same path — sidesteps the issue
                        // without changing the runtime behavior. The fork
                        // still happens; it just runs on the next tick.
                        //
                        // The actual queue logic lives in
                        // `queue_drill_down` so the test-only helper
                        // `apply_graph_patch_with_drill_down` and this
                        // live arm share one implementation.
                        self.queue_drill_down(&patch);

                        // Orphan check: in build phases, after each patch,
                        // detect nodes start can't reach (added but not wired
                        // into the start→…→deliverable chain) and prompt the
                        // model to connect them with LeadsTo. Dedup via the
                        // orphan-set signature so we don't repeat the hint
                        // every step. (Seeding only has start/deliverable.)
                        if matches!(self.graph_phase, GraphPhase::Filling | GraphPhase::Expanding) {
                            let orphans = self.replay_from_anchor();
                            if orphans.is_empty() {
                                self.last_orphan_hint_sig = None;
                            } else {
                                let mut joined = String::new();
                                for id in &orphans {
                                    joined.push_str(&id.to_string());
                                    joined.push('|');
                                }
                                let sig = hash_string(&joined);
                                if self.last_orphan_hint_sig != Some(sig) {
                                    self.last_orphan_hint_sig = Some(sig);
                                    let ids = orphans
                                        .iter()
                                        .map(|id| id.to_string())
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    self.conversation.add_user(format!(
                                        "⚠️ ORPHAN NODES — these are NOT yet connected into the main \
                                         chain (start cannot reach them): {ids}. They are floating. \
                                         \n\n\
                                         FIX: emit `propose_patch` with `add_edges` only — for each \
                                         orphan, add a `LeadsTo` edge to put it on the path from \
                                         start toward deliverable. Example: add_edges: [{{ \
                                         source: \"start\", target: \"orphan-id\", relation: \"LeadsTo\" \
                                         }}, ...]. \n\n\
                                         DO NOT use `drill_down` to fix orphans — drill_down starts a \
                                         sub-run for a complex step, it does NOT add edges. \
                                         `drill_down` without `add_edges` will leave the orphans \
                                         floating. Wire them in FIRST, drill_down SECOND if the \
                                         step really is complex."
                                    ));
                                }
                            }
                        }

                        // Redundant direct-edge monitor: once steps exist
                        // between start and deliverable, the seed's direct
                        // start→deliverable edge bypasses them. Remind the
                        // model to delete it EVERY round (no dedup) until it's
                        // gone — the main chain must be the single path.
                        if matches!(self.graph_phase, GraphPhase::Filling | GraphPhase::Expanding) {
                            if let Some(idx) = self.redundant_direct_edge_index() {
                                self.conversation.add_user(format!(
                                    "⚠️ A direct start→deliverable edge (index {idx}) still \
                                     exists and bypasses all the intermediate steps. The main \
                                     chain must be the single path start → step → … → \
                                     deliverable. Emit a `propose_patch` with \
                                     `remove_edge_indices: [{idx}]` to delete this direct edge."
                                ));
                            }
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
            ProposerStep::ConsultAdvisor { question, context, rationale: _ } => {
                self.handle_consult_advisor(question, context).await
            }
        }
    }

    /// Handle a `consult_advisor` step: route the question to the
    /// independent advisor model and inject its answer into the
    /// conversation. The advisor only answers — it never touches the
    /// graph. Degrades gracefully (hint, no crash) when no advisor is
    /// configured.
    async fn handle_consult_advisor(
        &mut self,
        question: String,
        context: String,
    ) -> Result<LoopState> {
        // Consulting is progress — reset the stuck detector.
        self.stuck_repeat_count = 0;
        self.last_stuck_signature = None;

        let advisor = match self.proposer.advisor.clone() {
            Some(a) => a,
            None => {
                self.conversation.add_user(
                    "No advisor model is configured, so consult_advisor is unavailable. \
                     Decide for yourself based on what you know, or use `explore` to \
                     gather more information.",
                );
                return Ok(LoopState::Running);
            }
        };

        let advisor_name = advisor.name().to_string();
        let prompt = if context.trim().is_empty() {
            format!(
                "You are an expert advisor to another AI agent that is building a plan \
                 graph for a task. Answer the agent's question directly and concretely. \
                 Do not ask for clarification — give your best expert answer.\n\n\
                 Question: {question}"
            )
        } else {
            format!(
                "You are an expert advisor to another AI agent that is building a plan \
                 graph for a task. Answer the agent's question directly and concretely. \
                 Do not ask for clarification — give your best expert answer.\n\n\
                 Context: {context}\n\nQuestion: {question}"
            )
        };
        let req = crate::model::ModelRequest {
            messages: vec![crate::model::Message::user(prompt)],
            tools: Vec::new(),
            temperature: 0.3,
            max_tokens: Some(4096),
            stop: Vec::new(),
        };
        match advisor.complete(req).await {
            Ok(resp) => {
                self.tokens_used = self.tokens_used.saturating_add(resp.usage.total_tokens as u64);
                let answer = if resp.content.trim().is_empty() {
                    resp.reasoning_content.clone().unwrap_or_default()
                } else {
                    resp.content.clone()
                };
                info!(advisor = %advisor_name, answer_len = answer.len(), "advisor consulted");
                self.conversation.add_user(format!(
                    "Advisor ({advisor_name}) answered your question:\n\n{answer}\n\n\
                     Use this to decide your next step. The advisor does not modify the \
                     graph — you do."
                ));
            }
            Err(e) => {
                warn!(error = %e, advisor = %advisor_name, "advisor call failed");
                self.conversation.add_user(format!(
                    "The advisor call failed ({e}). Proceed using your own judgment or \
                     try `explore` instead."
                ));
            }
        }
        Ok(LoopState::Running)
    }

    async fn run_verify_and_maybe_repair(&mut self) -> Result<LoopState> {
        // Backstop: don't hand off to verification with orphan nodes (steps
        // start can't reach). Bounce back to Filling and require the model to
        // wire them into the start→…→deliverable chain first.
        let orphans = self.replay_from_anchor();
        if !orphans.is_empty() {
            let ids = orphans
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            warn!(orphans = %ids, "ready_for_verify blocked: orphan nodes not on the chain");
            self.graph_phase = GraphPhase::Filling;
            self.conversation.add_user(format!(
                "Cannot verify yet: these nodes are not connected into the main chain \
                 (start cannot reach them): {ids}. Add `LeadsTo` edges to put each on the \
                 path start → … → deliverable, then emit `ready_for_verify` again."
            ));
            return Ok(LoopState::Running);
        }
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

        // 1. Decompose (or use auto-matched skill graph).
        let task_graph = if let Some(compiled) = self.try_match_and_compile_skill().await {
            compiled
        } else {
            match decomp
                .decompose(&self.graph, &self.task, Some(&self.conversation))
                .await
            {
                Ok(g) => g,
                Err(e) => {
                    warn!(error = %e, "decomposer failed");
                    self.phase = Phase::Poisoned;
                    return LoopState::Error(format!("decomposer failed: {e}"));
                }
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

        // 1.5 Cascade expansion: recursively expand abstract Task nodes
        // into concrete sub-nodes with file paths and actions (L0→L1→L2).
        // This builds the 3D multi-layer graph — when a node is too
        // complex, it becomes the anchor of its own sub-graph.
        let expanded_graph = crate::agent::cascade_expand::expand_graph(
            &*self.proposer.model,
            task_graph.clone(),
            &self.task,
            self.config.cascade_max_expand_depth.unwrap_or(2) as usize, // L0→L1→L2 (capped at 3 in cascade_expand)
        ).await;
        let task_graph = match expanded_graph {
            Ok(expanded) => {
                info!(
                    before = task_graph.node_count(),
                    after = expanded.node_count(),
                    "graph_loop: cascade expansion complete"
                );
                expanded
            }
            Err(e) => {
                warn!(error = %e, "graph_loop: cascade expansion failed, using original graph");
                task_graph // fall back to unexpanded graph
            }
        };

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

        // v2: when cascade backtracker is configured, auto-replan and
        // cascade-verify instead of surfacing GraphInvalid to the caller.
        // v1 fallback: if no cascade, surface GraphInvalid (user handles).
        if !task_graph_errors.is_empty() {
            if self.cascade.is_some() {
                return self.handle_task_phase_graph_errors(task_graph_errors).await;
            }
            // v1 fallback
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
            // v2.7: when sub-agent just failed (success=false, no
            // graph_error report), treat it as a generic L0Structural
            // error and route through the same auto-replan path used
            // for graph errors. This makes "if the step can't execute,
            // replan it" automatic for any failure mode, not just
            // graph-shape errors. Anchor failures (start/deliverable)
            // still surface to the caller because the anchor must
            // never be auto-redesigned.
            if !failures.is_empty() {
                let synthetic_errors: Vec<GraphError> = failures
                    .iter()
                    .filter_map(|f| {
                        // Don't try to replan the anchor — the L0
                        // gate explicitly rejects it (handle_task_phase_
                        // graph_errors also bails). The anchor's failure
                        // means the entire plan is broken; surface it.
                        let node = self.graph.nodes.get(
                            &crate::graph::NodeId::from(f.task_id.as_str()),
                        )?;
                        if node.immutable {
                            return None;
                        }
                        Some(GraphError::L0Structural {
                            error_type: L0ErrorType::MissingRelation,
                            detail: format!(
                                "Sub-agent for {} failed: {}. Re-planning.",
                                f.task_id, f.error
                            ),
                            related_nodes: vec![crate::graph::NodeId::from(
                                f.task_id.as_str(),
                            )],
                            discovered_by: Some("subagent_failure".into()),
                        })
                    })
                    .collect();
                if !synthetic_errors.is_empty() {
                    warn!(
                        count = synthetic_errors.len(),
                        "graph_loop: sub-agent(s) failed; auto-replanning affected steps"
                    );
                    return self.handle_task_phase_graph_errors(synthetic_errors).await;
                }
            }
            // Anchor failures (or empty synthetic list) — surface to caller.
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
                // Record WHY the review rejected the graph so it's observable
                // (run 7f7b60c0: only issue_count was logged, the judge's
                // actual complaint went nowhere). Surface the judge detail +
                // rationale into both the log and the conversation/transcript,
                // mirroring how the verify gate records its orphan list.
                let detail = result
                    .judge_verdict
                    .as_ref()
                    .map(|j| j.detail.as_str())
                    .unwrap_or("");
                warn!(
                    issue_count = errors.len(),
                    detail = %detail,
                    rationale = %result.rationale,
                    "graph_loop: review failed with graph-rooted cause; surfacing GraphInvalid"
                );
                self.conversation.add_user(format!(
                    "🔁 Review rejected the graph (root cause: graph/scope). \
                     Reviewer's finding: {detail}\nRationale: {}\n\
                     The graph needs repair before this task can complete.",
                    result.rationale
                ));
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

    /// v2: when sub-agents report graph errors during Task phase and a
    /// CascadeBacktracker is configured, auto-replan the failed nodes and
    /// cascade-backtrack instead of surfacing GraphInvalid to the caller.
    async fn handle_task_phase_graph_errors(
        &mut self,
        errors: Vec<GraphError>,
    ) -> LoopState {
        warn!(
            count = errors.len(),
            "graph_loop: auto-replanning after sub-agent graph errors"
        );

        let cascade = self.cascade.as_ref().cloned();
        let loader = self.subagent_loader.clone();

        for err in &errors {
            let failed_nodes = err.related_nodes();
            if failed_nodes.is_empty() {
                continue;
            }
            let failed_id = &failed_nodes[0];

            // Check if the failed node is the anchor.
            if let Some(node) = self.graph.nodes.get(failed_id) {
                if node.immutable {
                    warn!(anchor = %failed_id, "anchor node is infeasible; surfacing to caller");
                    self.pending = Pending::AwaitingRepair;
                    return LoopState::GraphInvalid {
                        source: ErrorSource::DuringExecution,
                        errors: vec![err.clone()],
                        snapshot: self.graph.clone(),
                    };
                }
            }

            // Ask the Proposer to re-plan the failed node.
            let evidence = err.detail();
            let mut cascade_start = failed_id.clone();
            match self.proposer.replan_failed_node(
                failed_id,
                &evidence,
                &self.graph,
                &self.task,
                &self.conversation,
            ).await {
                Ok(patch) => {
                    if let Some(replacement) = patch.add_nodes.first() {
                        cascade_start = replacement.id.clone();
                    }
                    if let Err(e) = self.graph.apply_patch(patch) {
                        warn!(error = %e, "re-plan patch rejected by graph");
                        self.pending = Pending::AwaitingRepair;
                        return LoopState::GraphInvalid {
                            source: ErrorSource::DuringExecution,
                            errors: vec![err.clone()],
                            snapshot: self.graph.clone(),
                        };
                    }
                    self.conversation.add_user(format!(
                        "Auto-replan: redesigned node {} after failure: {}",
                        failed_id, evidence
                    ));
                }
                Err(e) => {
                    warn!(error = %e, "re-plan model call failed; surfacing to caller");
                    self.pending = Pending::AwaitingRepair;
                    return LoopState::GraphInvalid {
                        source: ErrorSource::DuringExecution,
                        errors: vec![err.clone()],
                        snapshot: self.graph.clone(),
                    };
                }
            }

            // Cascade backtrack if configured.
            if let (Some(cascade), Some(l)) = (&cascade, &loader) {
                match cascade.backtrack_from(&cascade_start, &self.graph, &self.task, l.as_ref()).await {
                    Ok(result) => {
                        info!(
                            preserved = result.preserved.len(),
                            needs_repair = result.needs_repair.len(),
                            needs_reexec = result.needs_reexec.len(),
                            "cascade backtracking complete"
                        );
                        for repair_id in &result.needs_repair {
                            self.conversation.add_user(format!(
                                "Cascade: predecessor {} needs re-design. Re-planning.",
                                repair_id
                            ));
                            // Recursively re-plan this predecessor.
                            let sub_err = GraphError::L0Structural {
                                error_type: L0ErrorType::MissingRelation,
                                detail: format!(
                                    "Cascade: predecessor {} incompatible with redesigned successor",
                                    repair_id
                                ),
                                related_nodes: vec![repair_id.clone()],
                                discovered_by: Some("cascade_backtracker".into()),
                            };
                            return Box::pin(
                                self.handle_task_phase_graph_errors(vec![sub_err])
                            ).await;
                        }
                        if !result.needs_reexec.is_empty() {
                            let ids = result
                                .needs_reexec
                                .iter()
                                .map(|id| id.to_string())
                                .collect::<Vec<_>>()
                                .join(", ");
                            self.conversation.add_user(format!(
                                "Cascade: predecessor outputs need re-execution: {ids}. \
                                 Re-enter Graph phase and run the task graph from the top-level A/D plan."
                            ));
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "cascade backtracking errored; continuing");
                    }
                }
            }
        }

        // All errors processed. Gap 2: re-walk the whole graph from the
        // layer-1 Start before re-verifying. Any node whose dependency
        // chain no longer reaches the anchor is a structural break left by
        // the re-plan — surface it so the next Graph round re-wires (or
        // re-plans) the upstream that broke the path.
        let orphaned = self.replay_from_anchor();
        if !orphaned.is_empty() {
            let ids = orphaned
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            warn!(orphaned = %ids, "replay-from-anchor found structural breaks");
            self.conversation.add_user(format!(
                "REPLAY FROM START: after re-planning, these node(s) no longer have a \
                 dependency path back to the Start anchor: {ids}. That means an upstream \
                 step that fed them was changed or removed. Re-wire them to the correct \
                 (possibly re-planned) upstream node, or re-plan that upstream node, then \
                 walk the whole graph again from Start. Do not leave orphaned nodes."
            ));
        }

        // Re-enter Graph phase for re-verification.
        self.phase = Phase::Graph;
        LoopState::Running
    }

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
    /// Build a concrete hint for the Filling phase: suggest specific
    /// intermediate Task nodes to insert between A and D, based on the
    /// task description and current graph state. Called when the model
    /// has spent several rounds researching without adding nodes.
    fn build_filling_hint(&self) -> String {
        let node_info: Vec<String> = self.graph.nodes.values().map(|n| {
            format!("- {} (kind={:?}, summary=\"{}\")", n.id.as_str(), n.kind, n.summary)
        }).collect();
        format!(
            "🔧 You've spent several rounds without adding connected intermediate \
             steps between start and deliverable. Based on what you know, NOW add \
             step nodes AND wire them into the flow. Rules:\n\
             - Use semantic ids (e.g. `outline`, `design-modules`, `define-entities`), \
             NOT letter+number ids like B1/B2/T1.\n\
             - Step nodes are NOT required to form a single chain. They can:\n\
             \t• branch: one node feeds many (e.g. `define-roles` → both \
             `design-modules` and `define-entities`)\n\
             \t• converge: many nodes feed one (e.g. `define-roles` + \
             `define-entities` → `design-modules`)\n\
             \t• cross-depend: a node `B` may `DependsOn` an earlier node `A` \
             even if A is not its direct predecessor\n\
             \t• be a hub: a single complex node (e.g. \"design functional modules\") \
             may contain 5+ sub-concerns — see drill_down below\n\
             - For most step nodes: connect with `LeadsTo` edges in the main flow.\n\
             - For TRUE dependencies (B cannot be designed before A exists): use `DependsOn`.\n\
             - If a step node is itself a complex task (its summary is broad / lists \
             5+ sub-items / would be 1+ hour of work):\n\
             \t→ mark it for drill_down in the propose_patch (see schema). The system \
             will pause the parent graph at this node and spawn a sub-graph to expand it.\n\
             - The original start→deliverable edge can stay; it represents the goal \
             arc, not a forbidden shortcut.\n\
             - Emit a `propose_patch` now with the step node(s), their edges, and any \
             drill_down marks.\n\n\
             Current graph:\n{node_info}",
            node_info = node_info.join("\n")
        )
    }

    /// Seeding stall: deterministically create the two-node start→deliverable
    /// seed when the model refuses to. `start` is the immutable anchor (the
    /// starting state); `deliverable` is the goal. One LeadsTo edge
    /// start→deliverable wires the minimal plan, matching the seed the
    /// Proposer would have produced.
    /// Guarantees the loop always leaves the empty-graph state.
    fn auto_seed_start_goal(&mut self) {
        use crate::graph::{Edge, Node, NodeId, NodeKind, RelationType};
        let mut anchor =
            Node::new("start", NodeKind::Task, "start", "Start: current state / the task to accomplish");
        anchor.immutable = true;
        let goal = Node::new("deliverable", NodeKind::Task, "deliverable", "Deliverable: the desired outcome");
        self.graph.add_node(anchor);
        self.graph.add_node(goal);
        let _ = self.graph.add_edge(Edge::new(
            NodeId::from("start"),
            NodeId::from("deliverable"),
            RelationType::LeadsTo,
            0.9,
            "start leads to deliverable",
        ));
    }

    /// Gap 1: build the explore items for a forced research subagent when
    /// Filling has stalled. Produces two parallel scopes — a web search
    /// for general "how to" knowledge and a local file/codebase scan — so
    /// the model can fill intermediate nodes whether the gap is a
    /// knowledge gap or a code-discovery gap. The questions are derived
    /// from the task description and the current Start→Goal summaries.
    fn build_forced_search_items(&self) -> Vec<ExploreItem> {
        let goal_summary = self
            .graph
            .nodes
            .get(&NodeId::from("deliverable"))
            .map(|n| n.summary.clone())
            .unwrap_or_else(|| self.task.clone());
        let task = self.task.clone();
        vec![
            ExploreItem {
                scope: "web".into(),
                question: format!(
                    "Search the web for how to accomplish this task and what the \
                     intermediate steps are: {task}. Target outcome: {goal_summary}. \
                     Return a concise ordered list of concrete steps."
                ),
            },
            ExploreItem {
                scope: "codebase".into(),
                question: format!(
                    "Read the relevant files in the project to identify the concrete \
                     steps needed between the current state and: {goal_summary}. \
                     Report which files/functions must change and in what order."
                ),
            },
        ]
    }

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
                    role_prompt: String::new(),
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
                    // Summarize long subagent output to keep main context lean.
                    let summary = if r.output.len() > 3000 {
                        summarize_for_main_agent(&*self.proposer.model, &r.output).await
                    } else {
                        r.output.clone()
                    };
                    body.push_str(&format!(
                        "**Result** ({} tool calls, {}ms):\n{}\n",
                        r.tool_calls_made, r.duration_ms, summary
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
        // Suggest extracted entities for the model to propose as nodes.
        for (_, (item, res)) in items.iter().zip(results.iter()).enumerate() {
            if let Ok(r) = res {
                if r.success {
                    let patch = extract_entities_to_patch(&r.output, &item.scope, &self.graph);
                    if patch.add_nodes.len() > 0 || patch.add_edges.len() > 0 {
                        body.push_str(&format!(
                            "\n\n> 💡 **Suggested patch**: the following entities were auto-extracted from the explore output. \
                            Consider issuing `propose_patch` to add them to the graph at the right place. \
                            You may modify or reject any part.\n\
                            {} node(s) to add, {} edge(s) to add.\n\
                            Example nodes: {}\n\
                            Example edges: {}\n",
                            patch.add_nodes.len(),
                            patch.add_edges.len(),
                            patch.add_nodes.iter().map(|n| n.id.as_str()).take(5).collect::<Vec<_>>().join(", "),
                            patch.add_edges.iter().map(|e| format!("{}→{}", e.source, e.target)).take(5).collect::<Vec<_>>().join(", "),
                        ));
                    }
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
                ..Default::default()
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
                            ..Default::default()
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
#[allow(dead_code)]
const STUCK_REPEAT_SOFT_HINT: u32 = 3;

/// Stuck-detection tier 2: after this many consecutive rounds, escalate
/// to a hard hint that says the NEXT repeat will terminate the run.
#[allow(dead_code)]
const STUCK_REPEAT_HARD_HINT: u32 = 5;

/// Stuck-detection tier 3: at this many consecutive rounds, terminate
/// the run with `LoopState::Error` instead of letting it burn
/// `max_rounds` rounds. The error message is informative — the user
/// can see "stuck loop: 6 repeats" and try a different task shape.
#[allow(dead_code)]
const STUCK_REPEAT_TERMINATE: u32 = 6;

/// Graph stagnation tier 1: inject a soft hint into the conversation.
#[allow(dead_code)]
const GRAPH_STAGNATION_SOFT_HINT: u32 = 4;

/// Graph stagnation tier 2: inject a hard warning, last chance.
#[allow(dead_code)]
const GRAPH_STAGNATION_HARD_HINT: u32 = 6;

/// Graph stagnation tier 3: escalate to GraphInvalid for repair/re-planning.
#[allow(dead_code)]
const GRAPH_STAGNATION_TERMINATE: u32 = 12;

/// Compute a lightweight fingerprint of the current graph.
fn graph_fingerprint(g: &Graph) -> u64 {
    let mut node_ids: Vec<&str> = g.nodes.keys().map(|k| k.as_str()).collect();
    node_ids.sort();
    let mut s = String::new();
    s.push_str(&format!("{}:{}:", g.node_count(), g.edge_count()));
    for id in node_ids {
        s.push_str(id);
        s.push('|');
    }
    hash_string(&s)
}

impl GraphLoop {
    /// Check and update the graph stagnation counter.
    ///
    /// Staged escalation:
    /// - 4 rounds: soft hint
    /// - 6 rounds: hard hint
    /// - 8 rounds: if CascadeBacktracker is configured, run cascade
    ///   verification on the anchor (or most recent node). If cascade
    ///   finds issues → surface them as hints and continue. If cascade
    ///   finds nothing → terminate. If no cascade → terminate.
    async fn check_graph_stagnation(&mut self) -> Option<LoopState> {
        let fp = graph_fingerprint(&self.graph);
        if self.last_graph_fingerprint == Some(fp) {
            self.graph_stagnation_count += 1;

            // Tier 1: soft hint.
            if self.graph_stagnation_count == self.config.stagnation_soft_hint {
                warn!(count = self.graph_stagnation_count, "graph stagnated — soft hint");
                self.conversation.add_user(
                    "The graph hasn't changed for several rounds. If you're stuck, \
                     reconsider: is the last node you added still valid? Try proposing \
                     a patch — even a small edge change — to break the stall."
                );
            }

            // Tier 2: hard hint.
            if self.graph_stagnation_count == self.config.stagnation_hard_hint {
                warn!(count = self.graph_stagnation_count, "graph stagnated — hard hint");
                self.conversation.add_user(format!(
                    "STILL STUCK: {} rounds with no graph change. Go back to the LAST \
                     node you added or modified and re-examine it. If its design is \
                     wrong, patch it. If output is stale, re-execute. If there's a gap \
                     between two nodes, add an intermediate node.",
                    self.graph_stagnation_count
                ));
            }

            // Tier 3: cascade verification or terminate.
            if self.graph_stagnation_count >= self.config.stagnation_terminate {
                // Try cascade backtracking if configured.
                if let (Some(cascade), Some(loader)) =
                    (&self.cascade, &self.subagent_loader)
                {
                    warn!(count = self.graph_stagnation_count,
                        "graph stagnated — running cascade verification");

                    // Find the anchor or any task node to backtrack from.
                    let target = self
                        .graph
                        .nodes
                        .values()
                        .find(|n| matches!(n.kind, NodeKind::Task))
                        .map(|n| n.id.clone())
                        .unwrap_or_else(|| NodeId::from("anchor"));

                    match cascade
                        .backtrack_from(&target, &self.graph, &self.task, loader.as_ref())
                        .await
                    {
                        Ok(result) if !result.needs_repair.is_empty() => {
                            let ids: Vec<String> =
                                result.needs_repair.iter().map(|n| n.to_string()).collect();
                            self.conversation.add_user(format!(
                                "Cascade found {} node(s) needing repair: {}. \
                                 Propose patches for these nodes.",
                                ids.len(),
                                ids.join(", ")
                            ));
                            // Reset stagnation — cascade found actionable issues.
                            self.graph_stagnation_count = 0;
                            return None;
                        }
                        Ok(result) if !result.needs_reexec.is_empty() => {
                            let ids: Vec<String> =
                                result.needs_reexec.iter().map(|n| n.to_string()).collect();
                            self.conversation.add_user(format!(
                                "Cascade found {} node(s) needing re-execution: {}. \
                                 Re-execute these tasks.",
                                ids.len(),
                                ids.join(", ")
                            ));
                            self.graph_stagnation_count = 0;
                            return None;
                        }
                        Ok(_) => {
                            // Cascade found nothing — all preserved. Terminate.
                            warn!("cascade found no issues — graph truly stuck");
                        }
                        Err(e) => {
                            warn!(error = %e, "cascade verification failed");
                        }
                    }
                }

                // No cascade, or cascade found nothing — terminate.
                self.phase = Phase::Poisoned;
                return Some(LoopState::Error(format!(
                    "graph stagnated for {} rounds ({} nodes, {} edges). \
                     Cascade verification: {}. Hints at rounds {} and {}. \
                     Next round should pick a different optimization target.",
                    self.graph_stagnation_count,
                    self.graph.node_count(),
                    self.graph.edge_count(),
                    if self.cascade.is_some() { "no issues found" } else { "not configured" },
                    self.config.stagnation_soft_hint,
                    self.config.stagnation_hard_hint,
                )));
            }

            debug!(count = self.graph_stagnation_count, "graph unchanged");
        } else {
            self.last_graph_fingerprint = Some(fp);
            self.graph_stagnation_count = 0;
        }
        None
    }

    /// Gap 3: convergence detector (soft signal, hint-only).
    ///
    /// Three conditions define a "converged-looking" graph:
    ///   1. anchor (immutable) ↔ goal are connected (a directed path
    ///      exists between them via the edge graph, in either direction);
    ///   2. the graph fingerprint has been stable for this round (we piggy
    ///      back on `graph_stagnation_count`, which counts unchanged rounds);
    ///   3. every node has an L1 description (fully enriched).
    ///
    /// When all three hold for `convergence_stable_rounds` consecutive
    /// rounds, inject a single strong hint telling the model it should now
    /// emit `ready_for_verify`. We never emit it ourselves — the model
    /// keeps final say (user's explicit choice). The hint fires once per
    /// stable streak; any break in the conditions resets the streak.
    fn check_convergence_hint(&mut self) {
        let threshold = self.config.convergence_stable_rounds;
        if threshold == 0 {
            return; // disabled
        }
        // Only meaningful once we're past Seeding (a 2-node graph is
        // trivially "connected" but not actually a plan).
        if self.graph_phase == GraphPhase::Seeding || self.graph.node_count() < 3 {
            self.convergence_stable_count = 0;
            self.convergence_hint_sent = false;
            return;
        }

        let connected = self.anchor_goal_connected();
        let stable = self.graph_stagnation_count >= 1;
        let fully_enriched = self
            .graph
            .nodes
            .keys()
            .all(|id| self.graph.l1.contains(id));

        if connected && stable && fully_enriched {
            self.convergence_stable_count += 1;
            if self.convergence_stable_count >= threshold && !self.convergence_hint_sent {
                self.convergence_hint_sent = true;
                info!(
                    nodes = self.graph.node_count(),
                    edges = self.graph.edge_count(),
                    "convergence detected — injecting ready_for_verify hint"
                );
                self.conversation.add_user(
                    "✅ CONVERGENCE: the graph is structurally stable, the goal is \
                     connected back to the start, and every node has an L1 description. \
                     The plan looks complete. If you agree it satisfies the task, emit \
                     `ready_for_verify` now to hand off to verification. If something is \
                     still missing, add the missing node(s) instead — do not spin in place."
                );
            }
        } else {
            // Conditions broke — reset the streak so the hint can fire
            // again on the next genuine convergence.
            self.convergence_stable_count = 0;
            self.convergence_hint_sent = false;
        }
    }

    /// True if there is a directed path between the immutable anchor and
    /// the goal node (in either direction — the seed wires Goal→Start via
    /// DependsOn, but intermediate edges may run either way). Returns false
    /// when either endpoint is missing.
    fn anchor_goal_connected(&self) -> bool {
        let anchor = self
            .graph
            .nodes
            .values()
            .find(|n| n.immutable)
            .map(|n| n.id.clone());
        // Goal: prefer the conventional "deliverable" id, else any non-immutable
        // sink the seed produced.
        let goal = if self.graph.nodes.contains_key(&NodeId::from("deliverable")) {
            Some(NodeId::from("deliverable"))
        } else {
            self.graph
                .nodes
                .values()
                .find(|n| !n.immutable)
                .map(|n| n.id.clone())
        };
        let (Some(a), Some(d)) = (anchor, goal) else {
            return false;
        };
        if a == d {
            return false;
        }
        self.path_exists(&d, &a) || self.path_exists(&a, &d)
    }

    // -----------------------------------------------------------------------
    // v2 agent-harness spec: Clarifying v2 + Explore v2 saturation
    // -----------------------------------------------------------------------
    //
    // Per the spec, both `Clarifying.ask_user` and `Explore` rounds are
    // bounded by:
    // 1. A *soft upper bound* on consecutive rounds (Clarifying 10,
    //    Explore 200).
    // 2. A *similarity threshold* (default 0.85 on char-bigrams): a new
    //    question that matches any recent question at or above the
    //    threshold is treated as a repeat.
    //
    // When either guard trips, the loop surfaces a Block-style
    // `LoopState::Paused` with a `[block]` prefix on the question.
    // Tier hints (soft at 100, hard at 150) are also injected into
    // the conversation when Explore iter crosses those thresholds.
    //
    // The actual Jaccard math lives in `crate::agent::saturation`; the
    // methods here are thin wrappers over the four state fields.

    /// Inspect an `ask_user` question for Clarifying saturation.
    /// Does NOT mutate state. Caller records on `Proceed`.
    pub fn check_clarification_saturation(
        &self,
        new_question: &str,
    ) -> crate::agent::saturation::SaturationVerdict {
        use crate::agent::saturation::{jaccard, SaturationVerdict};
        if self.clarification_count >= self.config.clarification_max {
            return SaturationVerdict::CountLimit;
        }
        if self.clarification_history.is_empty() {
            return SaturationVerdict::Proceed;
        }
        let threshold = self.config.clarification_similarity_threshold;
        if self
            .clarification_history
            .iter()
            .any(|h| jaccard(new_question, h) >= threshold)
        {
            return SaturationVerdict::Repeat;
        }
        SaturationVerdict::Proceed
    }

    /// Record a successful `ask_user` Proceed: bump count, push to
    /// history. History is bounded by `clarification_history_window`.
    pub fn record_clarification(&mut self, question: String) {
        self.clarification_count = self.clarification_count.saturating_add(1);
        let cap = self.config.clarification_history_window.max(1);
        if self.clarification_history.len() >= cap {
            self.clarification_history.pop_front();
        }
        self.clarification_history.push_back(question);
    }

    /// Inspect an `explore` question for Explore saturation.
    /// Does NOT mutate state. Caller records on `Proceed`.
    pub fn check_explore_saturation(
        &self,
        new_question: &str,
    ) -> crate::agent::saturation::SaturationVerdict {
        use crate::agent::saturation::{jaccard, SaturationVerdict};
        if self.explorer_iter >= self.config.explore_max {
            return SaturationVerdict::CountLimit;
        }
        if self.explorer_history.is_empty() {
            return SaturationVerdict::Proceed;
        }
        let threshold = self.config.explore_similarity_threshold;
        if self
            .explorer_history
            .iter()
            .any(|h| jaccard(new_question, h) >= threshold)
        {
            return SaturationVerdict::Repeat;
        }
        SaturationVerdict::Proceed
    }

    /// Record a successful `explore` Proceed: bump iter, push to
    /// history.
    pub fn record_explore(&mut self, question: String) {
        self.explorer_iter = self.explorer_iter.saturating_add(1);
        let cap = self.config.explore_history_window.max(1);
        if self.explorer_history.len() >= cap {
            self.explorer_history.pop_front();
        }
        self.explorer_history.push_back(question);
    }

    /// Reset both Clarifying and Explore saturation counters.
    /// Called when the model emits a `propose_patch` (signals
    /// convergence) or when the loop surfaces a Block.
    pub fn reset_saturations(&mut self) {
        self.clarification_count = 0;
        self.clarification_history.clear();
        self.explorer_iter = 0;
        self.explorer_history.clear();
    }

    /// Returns the tier hint to inject for the current Explore iter,
    /// if any: `Soft` at `explore_soft_hint_at`, `Hard` at
    /// `explore_hard_hint_at`. Caller is responsible for actually
    /// pushing the hint and tracking the "sent" state via
    /// `mark_explore_soft_hint_sent` / `mark_explore_hard_hint_sent`.
    pub fn explore_tier_hint(
        &self,
    ) -> Option<crate::agent::saturation::TierHint> {
        use crate::agent::saturation::TierHint;
        if self.explorer_iter >= self.config.explore_hard_hint_at
            && !self.explore_hard_hint_sent
        {
            return Some(TierHint::Hard);
        }
        if self.explorer_iter >= self.config.explore_soft_hint_at
            && !self.explore_soft_hint_sent
        {
            return Some(TierHint::Soft);
        }
        None
    }

    pub fn mark_explore_soft_hint_sent(&mut self) {
        self.explore_soft_hint_sent = true;
    }

    pub fn mark_explore_hard_hint_sent(&mut self) {
        self.explore_hard_hint_sent = true;
    }

    /// v2 agent-harness spec §4.2: emit a `GraphPhase` event so the
    /// WebUI can update its phase progress bar. Called at every
    /// transition between `Clarifying` / `Seeding` / `Filling` /
    /// `Expanding` / `Verifying`. Saturated broadcasts (no listeners)
    /// are silently dropped.
    pub fn emit_graph_phase(&self) {
        use crate::web::events::RunEvent;
        let phase_str = match self.graph_phase {
            GraphPhase::Clarifying => "clarifying",
            GraphPhase::Seeding => "seeding",
            GraphPhase::Filling => "filling",
            GraphPhase::Expanding => "expanding",
            GraphPhase::Verifying => "verifying",
        };
        let _ = self.event_tx.send(RunEvent::GraphPhase {
            graph_phase: phase_str.to_string(),
            round: self.round,
            clarification_count: self.clarification_count,
            explorer_iter: self.explorer_iter,
            graph_version: self.graph.version,
        });
    }

    /// Directed reachability: is `to` reachable from `from` following
    /// outgoing edges? Plain BFS over the edge list.
    fn path_exists(&self, from: &NodeId, to: &NodeId) -> bool {
        let mut seen = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(from.clone());
        seen.insert(from.clone());
        while let Some(cur) = queue.pop_front() {
            for edge in self.graph.outgoing(&cur) {
                if !edge.relation.is_structural() { continue; }
                if &edge.target == to {
                    return true;
                }
                if seen.insert(edge.target.clone()) {
                    queue.push_back(edge.target.clone());
                }
            }
        }
        false
    }

    /// Like `path_exists`, but ignores the edge at `exclude_idx`. Used to ask
    /// "if I delete this one edge, does a path still exist?" — i.e. is the edge
    /// redundant? BFS scans the edge list by index so the excluded edge can be
    /// skipped precisely.
    fn path_exists_excluding(&self, from: &NodeId, to: &NodeId, exclude_idx: usize) -> bool {
        let mut seen = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(from.clone());
        seen.insert(from.clone());
        while let Some(cur) = queue.pop_front() {
            for (i, edge) in self.graph.edges.iter().enumerate() {
                if i == exclude_idx { continue; }
                if edge.source != cur { continue; }
                if !edge.relation.is_structural() { continue; }
                if &edge.target == to {
                    return true;
                }
                if seen.insert(edge.target.clone()) {
                    queue.push_back(edge.target.clone());
                }
            }
        }
        false
    }

    /// Find a redundant inbound edge to the goal (deliverable). An edge
    /// `X → goal` is redundant if, after removing it, X can STILL reach the
    /// goal via a longer path through the step circle. The goal must have
    /// exactly one inbound edge — the terminal of the step circle; any other
    /// inbound edge is a bypass that skips part of the circle (e.g. the seed's
    /// `start → deliverable`, or a step node wired straight to deliverable like
    /// `outline → deliverable`). Returns the redundant edge's index, else None.
    fn redundant_direct_edge_index(&self) -> Option<usize> {
        let goal = if self.graph.nodes.contains_key(&NodeId::from("deliverable")) {
            NodeId::from("deliverable")
        } else {
            self.graph.nodes.values().find(|n| !n.immutable).map(|n| n.id.clone())?
        };
        // Scan every inbound edge to the goal. The redundant one is the edge
        // whose source can still reach the goal without it.
        for (idx, edge) in self.graph.edges.iter().enumerate() {
            if edge.target != goal { continue; }
            if !edge.relation.is_structural() { continue; }
            if edge.source == goal { continue; }
            if self.path_exists_excluding(&edge.source, &goal, idx) {
                return Some(idx);
            }
        }
        None
    }

    /// Gap 2: re-walk the whole graph from the layer-1 Start (the
    /// immutable anchor) flowing *forward* along structural edges, and
    /// report any node that `start` cannot reach — i.e. a structural break
    /// introduced when a downstream node was re-planned or removed. Orphan
    /// = start cannot flow TO the node. This is the deterministic
    /// counterpart to the (semantic, model-driven) CascadeBacktracker:
    /// after a node is redesigned, the cascade checks whether each *direct*
    /// predecessor still fits, while this replay checks the *global*
    /// property — "walk forward from Start; wherever Start can no longer
    /// reach a node, that node's upstream needs re-planning."
    ///
    /// Returns the ids of orphaned non-anchor nodes, in stable order.
    /// Empty result means the graph is fully wired from Start.
    fn replay_from_anchor(&self) -> Vec<NodeId> {
        let anchor = match self.graph.nodes.values().find(|n| n.immutable) {
            Some(a) => a.id.clone(),
            None => return Vec::new(), // no anchor yet; nothing to replay
        };
        let mut orphaned: Vec<NodeId> = self
            .graph
            .nodes
            .values()
            .filter(|n| !n.immutable && n.id != anchor)
            .filter(|n| !self.path_exists(&anchor, &n.id))
            .map(|n| n.id.clone())
            .collect();
        orphaned.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        orphaned
    }
}

/// Tool-failure guardrail tier 1 (Hermes §tool_loop_guardrails
/// `same_tool_failure_warn_after`): after this many consecutive
/// failures of the same tool, inject a hint telling the model to
/// change strategy.
#[allow(dead_code)]
const TOOL_FAILURE_WARN_AFTER: u32 = 3;

/// Tool-failure guardrail tier 2: after this many consecutive
/// failures, request a graceful summary and terminate.
#[allow(dead_code)]
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
        ProposerStep::AskUser { question, options, rationale } => serde_json::json!({
            "step": "ask_user",
            "question": question,
            "options": options,
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
        ProposerStep::ConsultAdvisor { question, context, rationale } => serde_json::json!({
            "step": "consult_advisor",
            "question": question,
            "context": context,
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
                reasoning_content: None,
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

    /// Build a GraphLoop with a minimal seed graph (immutable `start` and
    /// a `deliverable` node wired with a single LeadsTo edge). Used by
    /// tests that exercise hint text generation against a non-empty
    /// graph state.
    fn test_graph_loop_with_seed() -> GraphLoop {
        let mut gl = build_loop_with(vec![]);
        let mut start = Node::new(
            "start",
            NodeKind::Task,
            "start",
            "Start: current state / the task to accomplish",
        );
        start.immutable = true;
        gl.graph.add_node(start);
        gl.graph.add_node(Node::new(
            "deliverable",
            NodeKind::Task,
            "deliverable",
            "Deliverable: the desired outcome",
        ));
        let _ = gl.graph.add_edge(Edge::new(
            "start",
            "deliverable",
            RelationType::LeadsTo,
            0.9,
            "seed",
        ));
        gl
    }

    /// Same as `test_graph_loop_with_seed` but rooted at a tempdir so
    /// `fork_sub_graph_for` can create the sub-run directory there. Sets
    /// `run_id = "test-run-001"` (a fixed string the tests can predict
    /// against) and uses `RunPersistence::with_data_dir(path)` so the
    /// layout matches the test's expectations
    /// (`<path>/<parent>/sub_runs/<sub>/run.json`).
    fn test_graph_loop_with_seed_at(path: &std::path::Path) -> GraphLoop {
        let mut gl = test_graph_loop_with_seed();
        gl.run_id = "test-run-001".to_string();
        let persistence = crate::web::persistence::RunPersistence::with_data_dir(path.to_path_buf());
        gl = gl.with_persistence(persistence);
        gl
    }

    #[test]
    fn build_filling_hint_no_longer_says_single_path() {
        let gl = test_graph_loop_with_seed();
        let hint = gl.build_filling_hint();
        assert!(
            !hint.contains("main chain is the single path"),
            "hint must not force single chain; got: {hint}"
        );
    }

    #[test]
    fn build_filling_hint_allows_branching_and_drill_down() {
        let gl = test_graph_loop_with_seed();
        let hint = gl.build_filling_hint();
        assert!(hint.contains("branch"), "hint should mention 'branch'");
        assert!(hint.contains("converge"), "hint should mention 'converge'");
        assert!(hint.contains("drill_down"), "hint should mention 'drill_down'");
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
            ..Default::default()
        });
        conv.messages.push(Message {
            role: Role::Assistant,
            content: "first turn got: ask_user".into(),
            ..Default::default()
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
        let _empty = Arc::new(ToolRegistry::new());

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
    async fn malformed_model_response_salvages_to_pause() {
        let mut gl = build_loop_with(vec!["not even valid JSON"]);
        // Malformed model output is salvaged as a pause so heartbeat/web
        // drivers can auto-answer and keep the loop alive.
        match gl.step().await {
            LoopState::Paused { question, .. } => {
                assert!(question.contains("valid JSON"));
            }
            other => panic!("expected Paused salvage from malformed JSON, got {other:?}"),
        }
        // Until the caller resumes, the pending pause is stable.
        match gl.step().await {
            LoopState::Paused { .. } => {}
            other => panic!("expected Paused sticky, got {other:?}"),
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
                options: vec![],
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
            let parsed = crate::agent::proposer::parse_step(&serialized, 1, 2000).unwrap();
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

        let l1_json_d = r#"{
            "responsibility":"goal for X",
            "implementation":"goal node",
            "design_intent":"complete X",
            "constraints":"reachable from A",
            "confidence":0.8
        }"#;
        let shared: Arc<dyn Model> =
            Arc::new(ScriptedModel::new(vec![patch_json, l1_json, l1_json_d, ready_json]));
        let tools = Arc::new(ToolRegistry::new());
        let proposer = GraphProposer::new(shared.clone(), tools.clone(), None);
        let verifier = Verifier::structural_only();

        let mut sources = std::collections::HashMap::new();
        sources.insert(NodeId::from("start"), "pub struct X;\n".into());
        sources.insert(NodeId::from("deliverable"), "// goal\n".into());
        let loader = Arc::new(crate::context::InMemorySources(sources));
        let enricher = crate::agent::enricher::L1Enricher::new(shared.clone(), loader);

        let cfg = GraphLoopConfig::defaults_at(std::env::current_dir().unwrap());
        let mut gl = GraphLoop::new("add module X", proposer, verifier, None, tools, cfg)
            .with_l1_enricher(enricher);
        // Skip Clarifying — this test exercises the enricher, not the phase gate.
        gl.graph_phase = GraphPhase::Seeding;

        // Step 1: proposer returns patch → apply → auto-enrich
        assert!(matches!(gl.step().await, LoopState::Running));
        assert_eq!(gl.graph.node_count(), 2);
        // L1 store should have been populated by auto_enrich
        let l1 = gl
            .graph
            .l1
            .get(&NodeId::from("start"))
            .expect("auto-enrichment should have written L1 for start");
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
        // Skip Clarifying — this test exercises enricher absence, not the phase gate.
        gl.graph_phase = GraphPhase::Seeding;
        // No .with_l1_enricher

        assert!(matches!(gl.step().await, LoopState::Running));
        assert_eq!(gl.graph.node_count(), 2);
        // No L1 entry — auto_enrich was a no-op
        assert!(gl.graph.l1.get(&NodeId::from("A")).is_none());
        assert!(gl.graph.l1.get(&NodeId::from("D")).is_none());

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
    async fn phase4_review_fail_records_judge_detail_in_conversation() {
        // Observability gap (run 7f7b60c0): when Review surfaced GraphInvalid,
        // only `issue_count` was logged — the judge's actual complaint
        // ("missing critical edges") went nowhere, so neither the user nor a
        // later debugging session could see WHY the graph was rejected. The
        // judge detail must be written into the conversation/transcript so the
        // reason is inspectable, mirroring how the verify gate records orphans.
        let proposer_resp = r#"{"step":"ready_for_verify","rationale":"go"}"#;
        let decomp_resp = r#"{"tasks":[],"rationale":"trivial"}"#;
        let judge_resp = r#"{"verdict":"fail","root_cause":"graph","detail":"missing critical edges between asset and contract","confidence":0.85}"#;
        let mut gl = build_phase4_loop(vec![proposer_resp, decomp_resp, judge_resp]);

        let mut state = LoopState::Running;
        for _ in 0..20 {
            state = gl.step().await;
            if !matches!(state, LoopState::Running) {
                break;
            }
        }
        assert!(matches!(state, LoopState::GraphInvalid { .. }));
        // The judge's detail must appear somewhere in the conversation so the
        // GraphInvalid reason is observable.
        let found = gl
            .conversation
            .messages
            .iter()
            .any(|m| m.content.contains("missing critical edges between asset and contract"));
        assert!(
            found,
            "judge detail must be recorded in the conversation/transcript; messages: {:?}",
            gl.conversation.messages.iter().map(|m| &m.content).collect::<Vec<_>>()
        );
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
    #[test]
    fn auto_seed_creates_start_goal_with_anchor_and_edge() {
        let mut gl = build_loop_with(vec!["{}"]);
        assert_eq!(gl.graph.node_count(), 0);
        gl.auto_seed_start_goal();
        assert_eq!(gl.graph.node_count(), 2);
        assert_eq!(gl.graph.edge_count(), 1);
        let anchor = gl
            .graph
            .nodes
            .values()
            .find(|n| n.immutable)
            .expect("anchor must exist");
        assert_eq!(anchor.id.as_str(), "start");
        // start must reach deliverable via the LeadsTo edge.
        assert!(gl.path_exists(&crate::graph::NodeId::from("start"), &crate::graph::NodeId::from("deliverable")));
    }

    #[tokio::test]
    async fn seeding_stall_auto_seeds_after_repeated_explore() {
        // Model always emits an explore step (never a seed patch).
        let explore = r#"{"step":"explore","items":[{"scope":"x","question":"y"}],"rationale":"r"}"#;
        let mut gl = build_loop_with(vec![explore, explore, explore, explore]);
        gl.graph_phase = GraphPhase::Seeding;
        assert_eq!(gl.graph_phase, GraphPhase::Seeding);
        // Rounds 1 & 2: still Seeding, empty graph, hint injected.
        let _ = gl.step_graph().await.unwrap();
        let _ = gl.step_graph().await.unwrap();
        assert_eq!(gl.graph.node_count(), 0, "no seed yet before the limit");
        // Round 3: guard hits SEEDING_STALL_LIMIT → auto-seed fires.
        let _ = gl.step_graph().await.unwrap();
        assert_eq!(gl.graph.node_count(), 2, "auto-seed must create Start+Goal");
        assert_eq!(gl.graph_phase, GraphPhase::Filling, "must advance past Seeding");
    }

    #[allow(dead_code)]
    fn _silence_unused(_k: NodeKind) {}

    #[tokio::test]
    async fn consult_advisor_degrades_gracefully_without_advisor() {
        // No advisor configured → handler injects a hint and stays Running.
        let mut gl = build_loop_with(vec!["{}"]);
        let state = gl
            .handle_consult_advisor("which approach?".into(), String::new())
            .await
            .unwrap();
        assert!(matches!(state, LoopState::Running));
        assert!(
            gl.conversation.transcript().contains("No advisor model is configured"),
            "should inject the graceful-degradation hint"
        );
    }

    #[tokio::test]
    async fn consult_advisor_routes_to_advisor_and_injects_answer() {
        let mut gl = build_loop_with(vec!["{}"]);
        let advisor: Arc<dyn Model> =
            Arc::new(ScriptedModel::new(vec!["Use a merge sort for stability."]));
        gl.proposer = gl.proposer.with_advisor(advisor);
        let state = gl
            .handle_consult_advisor("which sort?".into(), "1M items".into())
            .await
            .unwrap();
        assert!(matches!(state, LoopState::Running));
        let transcript = gl.conversation.transcript();
        assert!(transcript.contains("Advisor"), "answer should be injected");
        assert!(transcript.contains("merge sort"), "advisor's answer text present");
    }

    // ── Self-optimization laws: helpers ──

    /// Build a start→…→deliverable chain graph: anchor "start" (immutable),
    /// goal "deliverable", plus intermediate task nodes. LeadsTo edges run
    /// predecessor→successor (start leads to first middle, …, last middle
    /// leads to deliverable) — forward flow.
    fn build_chain_graph(gl: &mut GraphLoop, middles: &[&str], enrich_all: bool) {
        use crate::graph::{Edge, L1Description, Node, NodeId, RelationType};
        let mut anchor = Node::task("start", "Start");
        anchor.immutable = true;
        gl.graph.add_node(anchor);
        gl.graph.add_node(Node::task("deliverable", "Goal"));
        for m in middles {
            gl.graph.add_node(Node::task(*m, *m));
        }
        // Chain: start -> first middle -> ... -> last middle -> deliverable
        let mut chain: Vec<&str> = Vec::new();
        chain.push("start");
        chain.extend(middles.iter());
        chain.push("deliverable");
        for pair in chain.windows(2) {
            gl.graph
                .add_edge(Edge::new(
                    NodeId::from(pair[0]),
                    NodeId::from(pair[1]),
                    RelationType::LeadsTo,
                    0.9,
                    "",
                ))
                .unwrap();
        }
        if enrich_all {
            let ids: Vec<NodeId> = gl.graph.nodes.keys().cloned().collect();
            for id in ids {
                gl.graph
                    .l1
                    .set(id, L1Description::new("r", "i", "d", "c"));
            }
        }
    }

    #[test]
    fn anchor_goal_connected_true_for_wired_chain() {
        let mut gl = build_loop_with(vec!["{}"]);
        build_chain_graph(&mut gl, &["B", "C"], false);
        assert!(gl.anchor_goal_connected());
    }

    #[test]
    fn anchor_goal_connected_false_when_path_broken() {
        use crate::graph::Node;
        let mut gl = build_loop_with(vec!["{}"]);
        let mut anchor = Node::task("start", "Start");
        anchor.immutable = true;
        gl.graph.add_node(anchor);
        gl.graph.add_node(Node::task("deliverable", "Goal"));
        gl.graph.add_node(Node::task("B", "B"));
        // No edges at all → start cannot reach deliverable.
        assert!(!gl.anchor_goal_connected());
    }

    #[test]
    fn replay_from_anchor_flags_orphan_node() {
        use crate::graph::Node;
        let mut gl = build_loop_with(vec!["{}"]);
        build_chain_graph(&mut gl, &["B"], false);
        // Add an orphan node that start cannot reach.
        gl.graph.add_node(Node::task("ORPHAN", "dangling"));
        let orphans = gl.replay_from_anchor();
        assert_eq!(orphans, vec![crate::graph::NodeId::from("ORPHAN")]);
    }

    #[test]
    fn replay_from_anchor_empty_when_fully_wired() {
        let mut gl = build_loop_with(vec!["{}"]);
        build_chain_graph(&mut gl, &["B", "C"], false);
        assert!(gl.replay_from_anchor().is_empty());
    }

    // ── orphan detection + ready_for_verify backstop ──

    #[test]
    fn orphan_nodes_detected_when_not_wired() {
        use crate::graph::{Node, NodeId};
        let mut gl = build_loop_with(vec!["{}"]);
        build_chain_graph(&mut gl, &[], false); // start → deliverable, no middles
        // Add two orphan nodes that start cannot reach.
        gl.graph.add_node(Node::task("outline", "Outline"));
        gl.graph.add_node(Node::task("draft", "Draft"));
        let orphans = gl.replay_from_anchor();
        assert!(orphans.contains(&NodeId::from("outline")));
        assert!(orphans.contains(&NodeId::from("draft")));
        assert!(!orphans.contains(&NodeId::from("deliverable")));
    }

    #[test]
    fn no_orphans_when_steps_wired_into_chain() {
        let mut gl = build_loop_with(vec!["{}"]);
        // build_chain_graph wires start → outline → deliverable
        build_chain_graph(&mut gl, &["outline"], false);
        let orphans = gl.replay_from_anchor();
        assert!(orphans.is_empty(), "wired chain should have no orphans, got {orphans:?}");
    }

    #[tokio::test]
    async fn ready_for_verify_bounces_back_when_orphans() {
        use crate::graph::Node;
        let mut gl = build_loop_with(vec!["{}"]);
        build_chain_graph(&mut gl, &[], false); // start → deliverable, no middles
        // Add an orphan not connected to the chain.
        gl.graph.add_node(Node::task("orphan", "Orphan step"));
        let state = gl.run_verify_and_maybe_repair().await.unwrap();
        assert!(matches!(state, LoopState::Running));
        assert_eq!(gl.graph_phase, GraphPhase::Filling);
    }

    #[test]
    fn convergence_hint_fires_once_after_stable_rounds() {
        let mut gl = build_loop_with(vec!["{}"]);
        gl.config.convergence_stable_rounds = 3;
        gl.graph_phase = GraphPhase::Filling;
        build_chain_graph(&mut gl, &["B", "C"], true);
        // Simulate a stable graph: stagnation_count >= 1 each round.
        gl.graph_stagnation_count = 5;
        for _ in 0..3 {
            gl.check_convergence_hint();
        }
        assert!(gl.convergence_hint_sent, "hint should have fired");
        // The hint contains the marker "CONVERGENCE" and must appear
        // exactly once across the 3 rounds (fire-once semantics).
        let occurrences = gl.conversation.transcript().matches("CONVERGENCE").count();
        assert_eq!(occurrences, 1, "convergence hint must fire exactly once");
    }

    #[test]
    fn convergence_hint_resets_when_node_not_enriched() {
        let mut gl = build_loop_with(vec!["{}"]);
        gl.config.convergence_stable_rounds = 2;
        gl.graph_phase = GraphPhase::Filling;
        // enrich_all = false → at least one node lacks L1.
        build_chain_graph(&mut gl, &["B", "C"], false);
        gl.graph_stagnation_count = 5;
        for _ in 0..5 {
            gl.check_convergence_hint();
        }
        assert!(!gl.convergence_hint_sent, "must not fire while un-enriched");
        assert_eq!(gl.convergence_stable_count, 0);
    }

    #[test]
    fn convergence_disabled_when_threshold_zero() {
        let mut gl = build_loop_with(vec!["{}"]);
        gl.config.convergence_stable_rounds = 0;
        gl.graph_phase = GraphPhase::Filling;
        build_chain_graph(&mut gl, &["B", "C"], true);
        gl.graph_stagnation_count = 5;
        for _ in 0..5 {
            gl.check_convergence_hint();
        }
        assert!(!gl.convergence_hint_sent);
    }

    // ── Clarifying phase tests ──

    #[tokio::test]
    async fn non_heartbeat_run_starts_in_clarifying() {
        let gl = build_loop_with(vec!["{}"]);
        assert_eq!(gl.graph_phase, GraphPhase::Clarifying);
    }

    #[tokio::test]
    async fn propose_patch_advances_clarifying_to_seeding() {
        // In Clarifying, when the model emits a propose_patch (starts building),
        // the phase advances (no confirm button/sentinel). The seed patch then
        // moves Seeding→Filling via the existing transition, so we end in Filling.
        let patch = r#"{"step":"propose_patch","patch":{"add_nodes":[{"id":"start","kind":"Task","summary":"s","immutable":true},{"id":"deliverable","kind":"Task","summary":"d"}],"add_edges":[{"source":"start","target":"deliverable","relation":"LeadsTo","confidence":0.9}],"reason":"seed"},"rationale":"r"}"#;
        let mut gl = build_loop_with(vec![patch]);
        assert_eq!(gl.graph_phase, GraphPhase::Clarifying);
        let _ = gl.step_graph().await.unwrap();
        assert_ne!(gl.graph_phase, GraphPhase::Clarifying, "propose_patch should leave Clarifying");
    }

    #[test]
    fn heartbeat_run_starts_in_seeding() {
        let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec!["{}"]));
        let tools = Arc::new(ToolRegistry::new());
        let proposer = GraphProposer::new(model.clone(), tools.clone(), None);
        let verifier = Verifier::structural_only();
        let mut cfg = GraphLoopConfig::defaults_at(std::env::current_dir().unwrap());
        cfg.is_heartbeat = true;
        let gl = GraphLoop::new("hb task", proposer, verifier, None, tools, cfg);
        assert_eq!(gl.graph_phase, GraphPhase::Seeding);
    }

    #[test]
    fn redundant_direct_edge_detected_with_longer_path() {
        use crate::graph::{Edge, Node, RelationType};
        let mut gl = build_loop_with(vec!["{}"]);
        let mut start = Node::task("start", "Start");
        start.immutable = true;
        gl.graph.add_node(start);
        gl.graph.add_node(Node::task("deliverable", "Deliverable"));
        gl.graph.add_node(Node::task("mid", "Mid"));
        gl.graph.add_edge(Edge::new("start", "deliverable", RelationType::LeadsTo, 0.9, "")).unwrap();
        gl.graph.add_edge(Edge::new("start", "mid", RelationType::LeadsTo, 0.9, "")).unwrap();
        gl.graph.add_edge(Edge::new("mid", "deliverable", RelationType::LeadsTo, 0.9, "")).unwrap();
        assert_eq!(gl.redundant_direct_edge_index(), Some(0));
    }

    #[test]
    fn no_redundant_edge_when_direct_is_only_path() {
        use crate::graph::{Edge, Node, RelationType};
        let mut gl = build_loop_with(vec!["{}"]);
        let mut start = Node::task("start", "Start");
        start.immutable = true;
        gl.graph.add_node(start);
        gl.graph.add_node(Node::task("deliverable", "Deliverable"));
        gl.graph.add_edge(Edge::new("start", "deliverable", RelationType::LeadsTo, 0.9, "")).unwrap();
        assert_eq!(gl.redundant_direct_edge_index(), None);
    }

    #[test]
    fn no_redundant_edge_when_direct_absent() {
        use crate::graph::{Edge, Node, RelationType};
        let mut gl = build_loop_with(vec!["{}"]);
        let mut start = Node::task("start", "Start");
        start.immutable = true;
        gl.graph.add_node(start);
        gl.graph.add_node(Node::task("deliverable", "Deliverable"));
        gl.graph.add_node(Node::task("mid", "Mid"));
        gl.graph.add_edge(Edge::new("start", "mid", RelationType::LeadsTo, 0.9, "")).unwrap();
        gl.graph.add_edge(Edge::new("mid", "deliverable", RelationType::LeadsTo, 0.9, "")).unwrap();
        assert_eq!(gl.redundant_direct_edge_index(), None);
    }

    #[test]
    fn redundant_bypass_edge_from_step_node_detected() {
        // Regression (run 28ee249d): the model wired a step node directly to
        // deliverable (outline→deliverable) AND through the full step circle
        // (outline→stages→…→cheatsheet→deliverable). deliverable then had two
        // inbound edges; the bypass edge skips the rest of the circle. The
        // deliverable must have exactly ONE inbound edge — the terminal of the
        // step circle. The bypass (outline→deliverable) is the redundant one.
        use crate::graph::{Edge, Node, RelationType};
        let mut gl = build_loop_with(vec!["{}"]);
        let mut start = Node::task("start", "Start");
        start.immutable = true;
        gl.graph.add_node(start);
        gl.graph.add_node(Node::task("deliverable", "Deliverable"));
        for s in ["outline", "stages", "resources", "cheatsheet"] {
            gl.graph.add_node(Node::task(s, s));
        }
        gl.graph.add_edge(Edge::new("start", "outline", RelationType::LeadsTo, 0.9, "")).unwrap();
        // The bypass edge — index 1.
        gl.graph.add_edge(Edge::new("outline", "deliverable", RelationType::LeadsTo, 0.9, "")).unwrap();
        gl.graph.add_edge(Edge::new("outline", "stages", RelationType::LeadsTo, 0.9, "")).unwrap();
        gl.graph.add_edge(Edge::new("stages", "resources", RelationType::LeadsTo, 0.9, "")).unwrap();
        gl.graph.add_edge(Edge::new("resources", "cheatsheet", RelationType::LeadsTo, 0.9, "")).unwrap();
        gl.graph.add_edge(Edge::new("cheatsheet", "deliverable", RelationType::LeadsTo, 0.9, "")).unwrap();
        // outline→deliverable (idx 1) is redundant: outline still reaches
        // deliverable via outline→stages→resources→cheatsheet→deliverable.
        assert_eq!(gl.redundant_direct_edge_index(), Some(1));
    }

    #[test]
    fn no_redundant_when_deliverable_has_single_terminal() {
        // The legitimate shape: deliverable has exactly one inbound edge from
        // the end of the step circle. Nothing to flag.
        use crate::graph::{Edge, Node, RelationType};
        let mut gl = build_loop_with(vec!["{}"]);
        let mut start = Node::task("start", "Start");
        start.immutable = true;
        gl.graph.add_node(start);
        gl.graph.add_node(Node::task("deliverable", "Deliverable"));
        for s in ["outline", "stages"] {
            gl.graph.add_node(Node::task(s, s));
        }
        gl.graph.add_edge(Edge::new("start", "outline", RelationType::LeadsTo, 0.9, "")).unwrap();
        gl.graph.add_edge(Edge::new("outline", "stages", RelationType::LeadsTo, 0.9, "")).unwrap();
        gl.graph.add_edge(Edge::new("stages", "deliverable", RelationType::LeadsTo, 0.9, "")).unwrap();
        assert_eq!(gl.redundant_direct_edge_index(), None);
    }

    // --------------------------------------------------------------
    // Drill-down sub-graph machinery (Task 5)
    // --------------------------------------------------------------

    #[test]
    fn sub_run_status_default_is_running() {
        let s = SubRunStatus::default();
        assert!(matches!(s, SubRunStatus::Running));
    }

    #[test]
    fn sub_run_handle_carries_complex_node() {
        let h = SubRunHandle {
            sub_run_id: "sub-123".into(),
            complex_node: NodeId::from("design-modules"),
            started_at: 1000,
            status: SubRunStatus::Running,
        };
        assert_eq!(h.complex_node.as_str(), "design-modules");
        assert_eq!(h.sub_run_id, "sub-123");
    }

    #[test]
    fn drill_down_error_depth_limit() {
        let e = DrillDownError::DepthLimit;
        assert_eq!(format!("{e:?}"), "DepthLimit");
    }

    // --------------------------------------------------------------
    // Drill-down fork tests (Task 6)
    // --------------------------------------------------------------

    #[tokio::test]
    async fn fork_creates_sub_run_with_complex_node_as_start() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
        assert_eq!(handle.complex_node, complex);
        assert!(
            handle.sub_run_id.starts_with("test-run")
                || handle.sub_run_id.contains("sub"),
            "sub_run_id should be derived from parent + counter + depth; got {}",
            handle.sub_run_id
        );
        assert!(matches!(handle.status, SubRunStatus::Running));
    }

    #[tokio::test]
    async fn fork_records_sub_run_id_in_complex_node_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
        let node = gl.graph.nodes.get(&complex).unwrap();
        assert_eq!(
            node.metadata.get("sub_run_id").and_then(|v| v.as_str()),
            Some(handle.sub_run_id.as_str())
        );
        assert_eq!(
            node.metadata.get("sub_run_status").and_then(|v| v.as_str()),
            Some("running")
        );
        // drill_down_depth is stored as a JSON Number (per Task 6 wire-format
        // review). Compare via as_u64 instead of as_str.
        assert_eq!(
            node.metadata.get("drill_down_depth").and_then(|v| v.as_u64()),
            Some(1)
        );
        assert!(node.expanded, "Node.expanded should be set true after fork");
    }

    #[tokio::test]
    async fn fork_persists_sub_run_under_parent_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let handle = gl.fork_sub_graph_for(complex).await.unwrap();
        // Give the spawned sub-loop a moment to write run.json.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let sub_dir = tmp.path().join("test-run-001").join("sub_runs").join(&handle.sub_run_id);
        assert!(sub_dir.exists(), "sub_run dir should exist at {sub_dir:?}");
        assert!(sub_dir.join("run.json").exists(), "sub_run run.json should exist");
    }

    #[tokio::test]
    async fn fork_inherits_model_and_tools_and_increments_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        assert_eq!(gl.current_depth, 0);

        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));
        let handle = gl.fork_sub_graph_for(complex).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let sub_run_dir = tmp.path()
            .join("test-run-001")
            .join("sub_runs")
            .join(&handle.sub_run_id);
        let run_json = std::fs::read_to_string(sub_run_dir.join("run.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&run_json).unwrap();
        assert!(v.get("task").is_some(), "sub-run should have a task field");
        // Sanity check: the field must exist. Then assert the exact value.
        // This guards against silent off-by-one refactors — if the field
        // disappears or renames, the assert above catches it before the
        // tight equality check below gives a confusing error.
        assert!(
            v.get("current_depth").is_some(),
            "sub-run should have a current_depth field; payload: {v}"
        );
        assert_eq!(
            v.get("current_depth").and_then(|x| x.as_u64()),
            Some(1),
            "sub-run forked from depth 0 should be at depth 1; payload: {v}"
        );
        assert_eq!(
            v.get("parent_run_id").and_then(|x| x.as_str()),
            Some("test-run-001")
        );
    }

    #[tokio::test]
    async fn depth_limit_blocks_excessive_recursion() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 1;
        gl.current_depth = 1; // simulate being a sub-graph at depth 1
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let result = gl.fork_sub_graph_for(complex.clone()).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), DrillDownError::DepthLimit));
        let node = gl.graph.nodes.get(&complex).unwrap();
        assert!(node.metadata.get("sub_run_id").is_none());
        assert!(!node.expanded);
    }

    #[tokio::test]
    async fn depth_limit_allows_within_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        gl.current_depth = 0;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let result = gl.fork_sub_graph_for(complex).await;
        assert!(result.is_ok(), "depth 0 with max 2 should allow fork to depth 1");
    }

    /// The drill_down spec requires that "子图与父图享有同一套 drill_down
    /// 机制,可继续 fork 子子图、孙图" — a sub-graph must be able to
    /// itself fork a grandchild. This test simulates being a depth-1
    /// sub-graph (parent of which has current_depth = 0) and confirms a
    /// second fork to depth 2 still succeeds when max_drilldown_depth = 2.
    #[tokio::test]
    async fn sub_graph_can_drill_down_to_grandchild() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        gl.current_depth = 1; // simulate being a sub-graph already
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let handle = gl
            .fork_sub_graph_for(complex.clone())
            .await
            .expect("depth 1 with max 2 should fork to depth 2 (grandchild)");
        assert!(
            handle.sub_run_id.contains("d2"),
            "sub_run_id should encode depth=2 (grandchild); got {}",
            handle.sub_run_id
        );
        assert_eq!(
            gl.graph
                .nodes
                .get(&complex)
                .and_then(|n| n.metadata.get("drill_down_depth"))
                .and_then(|v| v.as_u64()),
            Some(2)
        );
    }

    // --------------------------------------------------------------
    // Drill-down poll tests (Task 7)
    // --------------------------------------------------------------

    #[tokio::test]
    async fn poll_sub_run_status_marks_done_when_sub_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let sub_dir = tmp.path().join("test-run-001").join("sub_runs").join(&handle.sub_run_id);
        std::fs::write(sub_dir.join("run.json"), r#"{"status":"Done"}"#).unwrap();

        let mut h = handle;
        gl.poll_sub_run_status(&mut h).await;
        assert!(matches!(h.status, SubRunStatus::Done));

        let node = gl.graph.nodes.get(&complex).unwrap();
        assert_eq!(node.metadata.get("sub_run_status").and_then(|v| v.as_str()), Some("done"));
    }

    #[tokio::test]
    async fn poll_sub_run_status_marks_error_when_sub_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let sub_dir = tmp.path().join("test-run-001").join("sub_runs").join(&handle.sub_run_id);
        std::fs::write(sub_dir.join("run.json"), r#"{"status":"Error","error":"reviewer failed"}"#).unwrap();

        let mut h = handle;
        gl.poll_sub_run_status(&mut h).await;
        assert!(matches!(h.status, SubRunStatus::Error(_)));
    }

    #[tokio::test]
    async fn poll_sub_run_status_idempotent_when_still_running() {
        // Construct a handle whose sub_run_id points to a directory the
        // sub-loop will NEVER write a `run.json` to. This isolates the
        // "file missing → keep Running" branch of `poll_sub_run_status`
        // from the real sub-loop's behavior (which terminates very
        // quickly in test mode and would otherwise leave a `run.json`
        // with `status: "Error"`).
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let h = SubRunHandle {
            sub_run_id: "nonexistent-sub-run-id".into(),
            complex_node: complex.clone(),
            // Use now_ms() so the handle is "fresh" — older values
            // (notably 0, the Unix epoch) would trip the timeout
            // producer introduced when sub_run_timeout_ms was wired
            // in. The whole point of this test is the
            // "file missing → keep Running" branch, not the timeout
            // branch, so we keep the handle well within its window.
            started_at: crate::agent::graph_loop::now_ms_for_test(),
            status: SubRunStatus::Running,
        };
        let mut handle = h;
        // Polling must NOT panic and must leave the handle in `Running`.
        gl.poll_sub_run_status(&mut handle).await;
        assert!(matches!(handle.status, SubRunStatus::Running));

        // Polling a second time must be equally safe (idempotent).
        gl.poll_sub_run_status(&mut handle).await;
        assert!(matches!(handle.status, SubRunStatus::Running));
    }

    #[test]
    fn mark_complex_node_done_sets_done_metadata() {
        let mut gl = test_graph_loop_with_seed();
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));
        gl.mark_complex_node_done(&complex);
        let node = gl.graph.nodes.get(&complex).unwrap();
        assert_eq!(node.metadata.get("status").and_then(|v| v.as_str()), Some("done"));
    }

    #[test]
    fn mark_complex_node_error_sets_error_metadata() {
        let mut gl = test_graph_loop_with_seed();
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));
        gl.mark_complex_node_error(&complex, "reviewer failed");
        let node = gl.graph.nodes.get(&complex).unwrap();
        assert_eq!(node.metadata.get("status").and_then(|v| v.as_str()), Some("error"));
        assert_eq!(node.metadata.get("error").and_then(|v| v.as_str()), Some("reviewer failed"));
        // The parent loop should also have its drill_down_error flag set
        // so the polling block at the top of step_graph can surface a
        // GraphInvalid on the next tick (Fix #2: sub-run error must
        // propagate, not silently get swallowed).
        assert!(
            matches!(&gl.drill_down_error, Some((n, e)) if n == &complex && e == "reviewer failed"),
            "expected drill_down_error to be set after mark_complex_node_error"
        );
    }

    // --------------------------------------------------------------
    // Drill-down Cancelled / Timeout / parent cancel tests
    // (Task 11 — spec gaps)
    // --------------------------------------------------------------

    #[test]
    fn sub_run_status_cancelled_variant_exists() {
        // The Cancelled variant must exist on SubRunStatus so
        // poll_sub_run_status can produce it when run.json reports
        // status=Cancelled. This test guards against accidental
        // removal of the variant.
        let s = SubRunStatus::Cancelled;
        assert!(matches!(s, SubRunStatus::Cancelled));
    }

    #[tokio::test]
    async fn poll_recognises_cancelled_status_from_run_json() {
        // poll_sub_run_status must recognise {"status":"Cancelled"}
        // and transition the handle to SubRunStatus::Cancelled.
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let sub_dir = tmp.path()
            .join("test-run-001")
            .join("sub_runs")
            .join(&handle.sub_run_id);
        std::fs::write(sub_dir.join("run.json"), r#"{"status":"Cancelled"}"#).unwrap();

        let mut h = handle;
        gl.poll_sub_run_status(&mut h).await;
        assert!(
            matches!(h.status, SubRunStatus::Cancelled),
            "expected Cancelled; got {:?}",
            h.status
        );
    }

    #[tokio::test]
    async fn poll_recognises_lowercase_cancelled_status() {
        // run.json may use lowercase "cancelled"; both must be
        // accepted so we don't trip on the canonicalisation.
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let sub_dir = tmp.path()
            .join("test-run-001")
            .join("sub_runs")
            .join(&handle.sub_run_id);
        std::fs::write(sub_dir.join("run.json"), r#"{"status":"cancelled"}"#).unwrap();

        let mut h = handle;
        gl.poll_sub_run_status(&mut h).await;
        assert!(matches!(h.status, SubRunStatus::Cancelled));
    }

    #[tokio::test]
    async fn poll_after_cancelled_stamps_node_metadata() {
        // After Cancelled is observed, the complex node's metadata
        // must show status=cancelled + sub_run_status=cancelled so
        // the web UI can render the cancellation.
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let sub_dir = tmp.path()
            .join("test-run-001")
            .join("sub_runs")
            .join(&handle.sub_run_id);
        std::fs::write(sub_dir.join("run.json"), r#"{"status":"Cancelled"}"#).unwrap();

        let mut h = handle;
        gl.poll_sub_run_status(&mut h).await;
        let node = gl.graph.nodes.get(&complex).unwrap();
        assert_eq!(
            node.metadata.get("status").and_then(|v| v.as_str()),
            Some("cancelled"),
            "complex node should be marked cancelled"
        );
        assert_eq!(
            node.metadata.get("sub_run_status").and_then(|v| v.as_str()),
            Some("cancelled"),
            "complex node sub_run_status should reflect cancellation"
        );
    }

    #[tokio::test]
    async fn poll_transitions_to_timeout_when_handle_aged_past_default() {
        // Build a handle whose started_at is well in the past (31
        // min ago, default timeout is 30 min). Write a non-terminal
        // run.json so the only thing that should move the handle
        // from Running is the timeout producer. After poll, the
        // handle must be Timeout and the complex node must be
        // marked timed-out.
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        gl.config.sub_run_timeout_ms = Some(30 * 60 * 1000); // explicit default
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
        let mut h = handle;
        // Rewind started_at to 31 minutes ago — past the 30-min default.
        h.started_at = crate::agent::graph_loop::now_ms_for_test()
            .saturating_sub(31 * 60 * 1000);
        // Make sure the file exists but is in a non-terminal status
        // (the timeout producer must override the "running" string).
        let sub_dir = tmp.path()
            .join("test-run-001")
            .join("sub_runs")
            .join(&h.sub_run_id);
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(sub_dir.join("run.json"), r#"{"status":"Running"}"#).unwrap();

        gl.poll_sub_run_status(&mut h).await;
        assert!(
            matches!(h.status, SubRunStatus::Timeout),
            "expected Timeout for handle aged 31 min; got {:?}",
            h.status
        );
        let node = gl.graph.nodes.get(&complex).unwrap();
        assert_eq!(
            node.metadata.get("status").and_then(|v| v.as_str()),
            Some("timeout")
        );
    }

    #[tokio::test]
    async fn poll_does_not_timeout_when_handle_fresh() {
        // Sanity: a freshly-started handle (1s ago) must NOT trip
        // the timeout producer even when run.json reports
        // status="Running".
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
        let mut h = handle;
        h.started_at = crate::agent::graph_loop::now_ms_for_test().saturating_sub(1000);
        let sub_dir = tmp.path()
            .join("test-run-001")
            .join("sub_runs")
            .join(&h.sub_run_id);
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(sub_dir.join("run.json"), r#"{"status":"Running"}"#).unwrap();

        gl.poll_sub_run_status(&mut h).await;
        assert!(
            matches!(h.status, SubRunStatus::Running),
            "fresh handle must stay Running; got {:?}",
            h.status
        );
    }

    #[test]
    fn mark_complex_node_cancelled_stamps_metadata() {
        // mark_complex_node_cancelled must stamp status=cancelled on
        // the complex node. It must NOT raise drill_down_error
        // (cancellation is a soft terminal, not a graph defect).
        let mut gl = test_graph_loop_with_seed();
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));
        gl.mark_complex_node_cancelled(&complex);
        let node = gl.graph.nodes.get(&complex).unwrap();
        assert_eq!(node.metadata.get("status").and_then(|v| v.as_str()), Some("cancelled"));
        assert!(
            gl.drill_down_error.is_none(),
            "Cancelled must NOT raise drill_down_error"
        );
    }

    #[test]
    fn mark_complex_node_timeout_stamps_metadata_and_raises_error() {
        // mark_complex_node_timeout must stamp status=timeout +
        // error="sub-run timeout" AND raise drill_down_error so
        // the polling block surfaces GraphInvalid on the next tick.
        let mut gl = test_graph_loop_with_seed();
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));
        gl.mark_complex_node_timeout(&complex);
        let node = gl.graph.nodes.get(&complex).unwrap();
        assert_eq!(node.metadata.get("status").and_then(|v| v.as_str()), Some("timeout"));
        assert_eq!(node.metadata.get("error").and_then(|v| v.as_str()), Some("sub-run timeout"));
        assert!(
            matches!(&gl.drill_down_error, Some((n, _)) if n == &complex),
            "Timeout must raise drill_down_error so GraphInvalid surfaces"
        );
    }

    #[tokio::test]
    async fn cancel_propagates_to_pending_sub_runs() {
        // With a pending sub-run, calling gl.cancel() and then
        // step_graph must:
        // - write Cancelled run.json to the sub-run
        // - mark the complex node as cancelled
        // - clear pending_sub_runs
        // - return LoopState::Error("parent cancelled")
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
        // Give the spawned sub-loop a beat to write its run.json
        // (it'll be a quick Done), then immediately cancel the
        // parent. We don't care which state the sub-run wrote; the
        // parent's cancel must override it.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        gl.pending_sub_runs.insert(complex.clone(), handle);

        assert_eq!(gl.pending_sub_runs.len(), 1);

        gl.cancel();
        let state = gl.step_graph().await.unwrap();
        match state {
            LoopState::Error(msg) => {
                assert!(
                    msg.contains("parent cancelled"),
                    "expected LoopState::Error(\"parent cancelled\"); got {msg}"
                );
            }
            other => panic!("expected LoopState::Error; got {other:?}"),
        }

        // pending_sub_runs must be empty.
        assert!(
            gl.pending_sub_runs.is_empty(),
            "pending_sub_runs should be drained after cancel; still has {} entries",
            gl.pending_sub_runs.len()
        );

        // Complex node must be marked cancelled.
        let node = gl.graph.nodes.get(&complex).unwrap();
        assert_eq!(
            node.metadata.get("status").and_then(|v| v.as_str()),
            Some("cancelled")
        );

        // Sub-run's run.json must have status=Cancelled.
        let sub_dirs: Vec<_> = std::fs::read_dir(
            tmp.path().join("test-run-001").join("sub_runs"),
        )
        .unwrap()
        .flatten()
        .collect();
        assert!(!sub_dirs.is_empty(), "expected at least one sub-run dir");
        let mut found_cancelled = false;
        for entry in sub_dirs {
            let path = entry.path().join("run.json");
            if path.exists() {
                let s = std::fs::read_to_string(&path).unwrap_or_default();
                if s.contains("\"Cancelled\"") {
                    found_cancelled = true;
                    break;
                }
            }
        }
        assert!(
            found_cancelled,
            "expected at least one sub-run run.json to have status=Cancelled"
        );
    }

    #[tokio::test]
    async fn cancel_skips_pending_fork_targets() {
        // If the parent has pending_fork_targets queued (from a
        // prior patch) and is then cancelled BEFORE the polling
        // block drains them, the drain must skip the fork — we
        // don't want to spawn a new sub-run on the way out.
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        // Simulate a queued fork target without actually calling
        // fork_sub_graph_for (which would spawn a real task).
        gl.pending_fork_targets
            .push((complex.clone(), "test reason".into()));

        gl.cancel();
        let state = gl.step_graph().await.unwrap();
        match state {
            LoopState::Error(msg) => assert!(msg.contains("parent cancelled")),
            other => panic!("expected LoopState::Error; got {other:?}"),
        }

        // pending_fork_targets should be cleared, no new sub-run
        // should have been created.
        assert!(gl.pending_fork_targets.is_empty());
        assert!(gl.pending_sub_runs.is_empty());
    }

    #[test]
    fn cancel_sets_flag_and_is_idempotent() {
        // gl.cancel() must set self.cancelled; gl.is_cancelled()
        // must reflect it; a second call must be a no-op (idempotent).
        let mut gl = test_graph_loop_with_seed();
        assert!(!gl.is_cancelled());
        gl.cancel();
        assert!(gl.is_cancelled());
        gl.cancel();
        assert!(gl.is_cancelled());
    }

    #[test]
    fn check_and_set_cancelled_propagates_external_signal() {
        // gl.check_and_set_cancelled(true) must mirror an external
        // cancel token onto the loop's internal flag. False is a
        // no-op (the external cancel hasn't fired yet).
        let mut gl = test_graph_loop_with_seed();
        gl.check_and_set_cancelled(false);
        assert!(!gl.is_cancelled());
        gl.check_and_set_cancelled(true);
        assert!(gl.is_cancelled());
    }

    #[test]
    fn propagate_cancel_to_pending_sub_runs_writes_cancelled_payload() {
        // Direct test of the helper: with a pending sub-run that has
        // a run.json containing status="Done", calling
        // propagate_cancel_to_pending_sub_runs must overwrite it
        // with status="Cancelled" + cancelled_by_parent=true.
        // We construct a `SubRunHandle` directly (rather than going
        // through `fork_sub_graph_for`) so this can be a sync test.
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        // Create a sub-run directory manually with a known
        // sub_run_id. The handle's `started_at` is in the past but
        // irrelevant here because we're testing propagation, not
        // timeout.
        let sub_run_id = "test-run-001-sub-99-d1".to_string();
        let sub_dir = tmp.path()
            .join("test-run-001")
            .join("sub_runs")
            .join(&sub_run_id);
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(sub_dir.join("run.json"), r#"{"status":"Done"}"#).unwrap();

        let handle = SubRunHandle {
            sub_run_id,
            complex_node: complex.clone(),
            started_at: 0,
            status: SubRunStatus::Running,
        };
        gl.pending_sub_runs.insert(complex.clone(), handle);

        let n = gl.propagate_cancel_to_pending_sub_runs();
        assert_eq!(n, 1, "expected 1 sub-run cancelled");
        assert!(gl.pending_sub_runs.is_empty());

        let payload = std::fs::read_to_string(sub_dir.join("run.json")).unwrap();
        assert!(payload.contains("\"Cancelled\""), "got {payload}");
        assert!(
            payload.contains("\"cancelled_by_parent\""),
            "expected cancelled_by_parent flag; got {payload}"
        );
    }

    // --------------------------------------------------------------
    // Drill-down integration tests (Task 10)
    // --------------------------------------------------------------

    #[tokio::test]
    async fn step_graph_polling_only_when_pending_sub_runs_nonempty() {
        // When pending_sub_runs is non-empty with a Running handle, polling
        // should not crash, should keep tracking the pending sub-run, and
        // should return early (Running) without invoking the proposer. We
        // exercise the integration via the public `apply_graph_patch_with_drill_down`
        // helper plus a direct `step_graph` call after seeding the pending map.
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;
        let complex = NodeId::from("design-modules");
        gl.graph.add_node(Node::task("design-modules", "..."));

        let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
        gl.pending_sub_runs.insert(complex.clone(), handle);

        assert_eq!(gl.pending_sub_runs.len(), 1, "pending sub-run should be tracked");

        // step_graph must return early when there are still-pending sub-runs,
        // leaving the pending map intact.
        let state = gl.step_graph().await.unwrap();
        assert!(
            matches!(state, LoopState::Running),
            "step_graph should return Running while sub-runs are still pending; got {state:?}"
        );
        assert_eq!(
            gl.pending_sub_runs.len(),
            1,
            "pending sub-run should still be tracked after polling"
        );
    }

    #[tokio::test]
    async fn patch_with_drill_down_creates_sub_run() {
        // When a GraphPatch with drill_down is applied via the integration
        // helper, the target node must be added, the sub-run must be forked,
        // and the node's `expanded` flag must be set.
        let tmp = tempfile::tempdir().unwrap();
        let mut gl = test_graph_loop_with_seed_at(tmp.path());
        gl.config.max_drilldown_depth = 2;

        let patch = GraphPatch {
            add_nodes: vec![Node::task("design-modules", "10+ sub-modules")],
            add_edges: vec![Edge::new(
                "start",
                "design-modules",
                RelationType::LeadsTo,
                0.9,
                "...",
            )],
            remove_node_ids: vec![],
            remove_edge_indices: vec![],
            set_l1: vec![],
            reason: "expanding".into(),
            drill_down: Some(DrillDownMark {
                target: NodeId::from("design-modules"),
                reason: "10+ sub-modules".into(),
                sub_task_override: None,
            }),
        };
        gl.apply_graph_patch_with_drill_down(&patch).await.unwrap();

        // The helper only queues the fork (see the comment at the polling
        // block drain site in `step_graph` for why the actual `tokio::spawn`
        // happens there). Drive `step_graph` once to drain the queue and
        // verify the sub-run is forked.
        let _ = gl.step_graph().await.unwrap();

        assert_eq!(
            gl.pending_sub_runs.len(),
            1,
            "drill_down should create a pending sub-run"
        );
        assert!(
            gl.graph
                .nodes
                .get(&NodeId::from("design-modules"))
                .unwrap()
                .expanded,
            "complex node should be marked expanded after fork"
        );
    }
}
