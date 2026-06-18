//! Crate-level error type.
//!
//! Every fallible operation returns [`Result<T>`], aliased over [`HarnessError`].
//! Each variant carries a stringified context payload so that the cause can be
//! logged or fed back into the GRAPH state for [[project-design-principles]]
//! principle #3 (local graph repair).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("graph: {0}")]
    Graph(String),

    #[error("scanner: {0}")]
    Scanner(String),

    #[error("scheduler: {0}")]
    Scheduler(String),

    #[error("context: {0}")]
    Context(String),

    #[error("domain: {0}")]
    Domain(String),

    #[error("model: {0}")]
    Model(String),

    #[error("enricher: {0}")]
    Enricher(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, HarnessError>;

impl HarnessError {
    pub fn graph(msg: impl Into<String>) -> Self {
        Self::Graph(msg.into())
    }

    pub fn scanner(msg: impl Into<String>) -> Self {
        Self::Scanner(msg.into())
    }

    pub fn scheduler(msg: impl Into<String>) -> Self {
        Self::Scheduler(msg.into())
    }

    pub fn context(msg: impl Into<String>) -> Self {
        Self::Context(msg.into())
    }

    pub fn domain(msg: impl Into<String>) -> Self {
        Self::Domain(msg.into())
    }

    pub fn model(msg: impl Into<String>) -> Self {
        Self::Model(msg.into())
    }

    pub fn enricher(msg: impl Into<String>) -> Self {
        Self::Enricher(msg.into())
    }
}
