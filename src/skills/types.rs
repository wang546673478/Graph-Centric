//! Skill data types: `Skill`, `SkillMeta`, `SkillRef`, `SkillError`.

use crate::error::HarnessError;
use crate::graph::{Graph, NodeId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A single captured skill: the full L0+L1 graph of a successful run, plus
/// provenance and a one-sentence trigger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Kebab-case short name (e.g., "plan-relocation-bjs-sha").
    pub slug: String,
    /// The original user task that produced this skill.
    pub task: String,
    /// One-sentence "This skill applies when..." description.
    pub trigger: String,
    /// The L0 + L1 graph of the run.
    pub graph: Graph,
    /// The review verdict that approved this skill for capture.
    pub review: serde_json::Value,
    /// Provenance / metadata.
    pub meta: SkillMeta,
}

/// Metadata for a saved skill. Stored in `meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    /// ISO 8601 timestamp of when this skill was captured.
    pub created_at: String,
    /// The task's NodeId in the original graph, if known.
    pub task_id: Option<NodeId>,
    /// Which model generated the slug and trigger.
    pub model_used: String,
    /// Domain tags derived from L0 node kinds (e.g., "code", "research").
    pub domain_tags: Vec<String>,
    /// Mean L1 confidence across all L1 entries; 0.0 if no L1.
    pub l1_avg_confidence: f64,
}

/// Lightweight reference used in prompts and listings. Carries only the
/// slug and the one-sentence trigger — no graph payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillRef {
    pub slug: String,
    pub trigger: String,
}

/// Errors from the skills module.
#[derive(Debug, Error)]
pub enum SkillError {
    #[error("skill not found: {0}")]
    NotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid slug (must match ^[a-z0-9-]+$): {0}")]
    InvalidSlug(String),
    #[error("model call failed: {0}")]
    Model(String),
    #[error("harness error: {0}")]
    Harness(#[from] HarnessError),
}

pub type Result<T> = std::result::Result<T, SkillError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_meta_serializes_to_json() {
        let meta = SkillMeta {
            created_at: "2026-06-03T12:00:00Z".to_string(),
            task_id: None,
            model_used: "test-model".to_string(),
            domain_tags: vec!["code".to_string()],
            l1_avg_confidence: 0.85,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: SkillMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.created_at, meta.created_at);
        assert_eq!(back.model_used, meta.model_used);
        assert_eq!(back.l1_avg_confidence, meta.l1_avg_confidence);
    }

    #[test]
    fn skill_ref_is_slug_plus_trigger() {
        let r = SkillRef {
            slug: "my-skill".to_string(),
            trigger: "This skill applies when...".to_string(),
        };
        // Equality + serialization are the only operations that need to
        // round-trip cleanly. No graph payload.
        let json = serde_json::to_string(&r).unwrap();
        let back: SkillRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn skill_error_implements_std_error() {
        let e = SkillError::NotFound("foo".to_string());
        // Just confirm Display works.
        assert!(format!("{e}").contains("foo"));
    }
}
