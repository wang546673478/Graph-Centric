//! Graph-Centric Agent Harness — library entry.
//!
//! This crate implements a domain-agnostic agent orchestration system built
//! around a single thesis: **all agent work is graph work**. Every concrete
//! problem is reduced to operations on a relationship graph whose nodes are
//! entities (files, services, datasets, concepts) and whose edges are the
//! real structural relations between them.
//!
//! The control flow is a fixed three-state machine — `GRAPH ↔ TASK ↔ REVIEW` —
//! intentionally rigid so that nothing else can drift. The relationship graph
//! is the dynamic, growing world model.
//!
//! Phase 1 (this milestone) ships only the deterministic substrate: graph
//! types, traversal, validation, DAG scheduling, context construction, and
//! domain trait surface. No model integration yet.

pub mod agent;
pub mod context;
pub mod domain;
pub mod error;
pub mod graph;
pub mod model;
pub mod scheduler;
pub mod skills;
pub mod tools;

pub use error::{HarnessError, Result};
pub use graph::{
    Edge, EdgeChange, Graph, GraphPatch, GraphStatus, Inconsistency, Node, NodeId, NodeKind,
    RelationType, Traversal,
};
pub use scheduler::{DagScheduler, Schedule};
