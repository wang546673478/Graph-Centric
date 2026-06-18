//! Skill capture & reuse: reify successful agent runs as reusable skills.
//!
//! See `docs/superpowers/specs/2026-06-03-skill-capture-and-reuse-design.md`
//! for the design rationale.

pub mod capture;
pub mod compiler;
pub mod matcher;
pub mod prompt_registry;
pub mod retrieve;
pub mod slug;
pub mod storage;
pub mod storage_composite;
pub mod storage_repo;
pub mod types;

pub use matcher::{find_matching_skills, score_skill_match};
pub use types::{Skill, SkillError, SkillMeta, SkillRef};
// Re-exports expanded in Tasks 4 (RepoSkillStorage) and 5 (CompositeSkillStorage).
pub use storage::{LocalSkillStorage, SkillStorage};
pub use storage_repo::RepoSkillStorage;
pub use storage_composite::CompositeSkillStorage;
