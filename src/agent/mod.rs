//! Agent layer — the iterative graph loop and the components that drive it.
//!
//! Per [[feedback-iterative-loop-is-centerpiece]], the system's identity is
//! a universal `input → reason → build/extend graph → verify → (repair) →
//! dispatch` loop. The same loop runs at every layer:
//!
//! - **Main agent**: input is a user task; "reasoning" happens through chat
//!   with the human; verification can include user confirmation.
//! - **Sub-agent**: input is a task + parent's subgraph slice; "reasoning"
//!   happens by reading source data and calling tools; verification is
//!   tool-driven (compile / test / typecheck / domain checks).
//!
//! This module owns the parts that are common to both:
//!
//! - [`Conversation`] — multi-turn state with the message history and a
//!   handle to the current graph.
//!
//! The remaining pieces — `GraphProposer`, `Verifier`, `LocalRepairer`,
//! `GraphLoop` — land alongside this as Phase 2 progresses.

pub mod conversation;
pub mod decomposer;
pub mod dispatcher;
pub mod enricher;
pub mod graph_loop;
pub mod intake;
pub mod proposer;
pub mod repairer;
pub mod reviewer;
pub mod subagent;
pub mod validator;
pub mod verifier;

pub use conversation::Conversation;
pub use decomposer::Decomposer;
pub use dispatcher::{DispatchOutcome, Dispatcher, DispatcherConfig, SubAgentPool};
pub use enricher::L1Enricher;
pub use graph_loop::{
    ErrorSource, FinalResult, GraphError, GraphLoop, GraphLoopConfig, L0ErrorType, LoopState,
    SubTaskFailure,
};
pub use intake::{TaskClarity, check_intake_compliance, classify_task_clarity};
pub use proposer::{GraphProposer, ProposerStep, extract_json_block, parse_step};
pub use repairer::LocalRepairer;
pub use reviewer::{JudgeVerdict, ReviewResult, Reviewer, RootCause};
pub use subagent::{SubAgent, SubAgentResult, SubTask};
pub use validator::{
    AlwaysPasses, BashCheckValidator, PostExecutionValidator, ValidationVerdict,
};
pub use verifier::{IssueSource, Severity, VerificationResult, VerifyIssue, Verifier};
