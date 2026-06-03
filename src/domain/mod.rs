//! Domain abstraction — the single injection point that lets the same
//! harness serve code, data, infra, research, or business-process domains.
//!
//! Per design doc §10 and design principle #4: the harness is generic;
//! domain knowledge enters via the four traits in this module. Adding a
//! new domain is implementing these traits, never editing the orchestrator.

pub mod code;

use crate::error::Result;
use crate::graph::Graph;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Canonical ToolDef lives in `crate::tools` alongside the executable Tool
// trait. Re-export it so existing `domain::ToolDef` paths keep compiling.
pub use crate::tools::ToolDef;

/// What a task is allowed to do. The orchestrator uses this to assemble
/// the tool set passed to each sub-agent (design doc §7.2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskNeeds {
    #[serde(default)]
    pub can_read: bool,
    #[serde(default)]
    pub can_write: bool,
    #[serde(default)]
    pub can_execute: bool,
    /// Free-form capability flags for domain-specific tools.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extras: HashMap<String, serde_json::Value>,
}

impl TaskNeeds {
    pub fn read_only() -> Self {
        Self {
            can_read: true,
            ..Default::default()
        }
    }
    pub fn read_write() -> Self {
        Self {
            can_read: true,
            can_write: true,
            ..Default::default()
        }
    }
    pub fn full() -> Self {
        Self {
            can_read: true,
            can_write: true,
            can_execute: true,
            ..Default::default()
        }
    }
}

/// Opaque tool descriptor — *deprecated alias*. The canonical definition is
/// [`crate::tools::ToolDef`]; the `pub use` at the top of this module keeps
/// existing call sites (`domain::ToolDef`) compiling. New code should
/// import from `crate::tools`.

/// Outcome of running a task's deterministic post-conditions (tests, lint,
/// schema validation, etc.). Per design principle #5, the LLM-as-judge is
/// only allowed to weigh in after these have passed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationOutcome {
    pub passed: bool,
    pub checks: Vec<CheckResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub details: String,
}

// ---------------------------------------------------------------------------
// Trait surface
// ---------------------------------------------------------------------------

/// Build the initial relationship graph from raw domain data.
/// E.g. for code: walk the project directory and parse imports/calls.
#[async_trait]
pub trait Scanner: Send + Sync {
    /// `source` is opaque — a project root path for code, a connection
    /// string for data, an HCL directory for infra, etc.
    async fn scan(&self, source: &str) -> Result<Graph>;
}

/// Build the tool set a sub-agent is allowed to use based on its needs.
pub trait ToolRegistry: Send + Sync {
    fn build_tools(&self, needs: &TaskNeeds) -> Vec<ToolDef>;
}

/// Run domain-specific deterministic checks against the current world
/// state after a batch of tasks completes. This is the deterministic
/// backstop required by design principle #5.
#[async_trait]
pub trait DomainValidator: Send + Sync {
    async fn validate(&self, graph: &Graph) -> Result<ValidationOutcome>;
}

/// A complete domain bundle. The orchestrator holds one of these to know
/// how to enter the GRAPH state and how to back-stop REVIEW.
pub struct Domain {
    pub name: String,
    pub scanner: Box<dyn Scanner>,
    pub tool_registry: Box<dyn ToolRegistry>,
    pub validator: Box<dyn DomainValidator>,
}

impl std::fmt::Debug for Domain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Domain").field("name", &self.name).finish()
    }
}
