//! LLM-based slug generation, with deterministic hash fallback.

use super::types::Result;
use crate::model::Model;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Generate a kebab-case slug for a skill, via the fast model. Falls back
/// to `task-<hash>` if the model returns something invalid.
///
/// The prompt is fixed and demands a single-line reply; the model is
/// expected to return only the slug (no markdown fences, no prose).
pub async fn generate_slug(
    model: Arc<dyn Model>,
    task: &str,
    graph_summary: &str,
) -> Result<String> {
    let prompt = format!(
        "Task: {task}\n\n\
         Graph summary: {graph_summary}\n\n\
         Generate a 3-5 word kebab-case slug (lowercase, hyphens only) that \
         names this skill. Examples: plan-relocation-bjs-sha, \
         refactor-rust-traits, cargo-build-debug, write-marketing-blog.\n\n\
         Output ONLY the slug. No prose, no quotes, no markdown."
    );

    let request = crate::model::ModelRequest {
        messages: vec![crate::model::Message::user(prompt)],
        tools: Vec::new(),
        temperature: 0.3,
        max_tokens: Some(32),
        stop: Vec::new(),
    };

    let response = model
        .complete(request)
        .await
        .map_err(|e| super::types::SkillError::Model(format!("generate_slug: {e}")))?;

    let raw = response.content.trim().to_string();
    if is_valid_slug(&raw) {
        return Ok(raw);
    }
    Ok(fallback_slug(task))
}

/// True if `s` matches `^[a-z0-9-]+$` (after trimming) and has at least one char.
fn is_valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Deterministic fallback: `task-<16-hex hash of task>`. Never collides
/// with LLM slugs in practice (LLMs use semantic words; this is hex).
fn fallback_slug(task: &str) -> String {
    let mut hasher = DefaultHasher::new();
    task.hash(&mut hasher);
    format!("task-{:x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::HarnessError;
    use crate::model::{FinishReason, Model, ModelRequest, ModelResponse, Usage};
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct MockModel {
        responses: Mutex<Vec<String>>,
    }

    impl MockModel {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().rev().map(String::from).collect()),
            }
        }
    }

    #[async_trait]
    impl Model for MockModel {
        fn name(&self) -> &str {
            "mock"
        }
        async fn complete(
            &self,
            _req: ModelRequest,
        ) -> std::result::Result<ModelResponse, HarnessError> {
            let content = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| "default-slug".to_string());
            Ok(ModelResponse {
                content,
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                reasoning_content: None,
                usage: Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn generate_slug_uses_model_response_when_valid() {
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec!["my-cool-skill"]));
        let s = generate_slug(m, "do the thing", "5 nodes").await.unwrap();
        assert_eq!(s, "my-cool-skill");
    }

    #[tokio::test]
    async fn generate_slug_trims_whitespace() {
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec!["  trimmed-slug  \n"]));
        let s = generate_slug(m, "task", "graph").await.unwrap();
        assert_eq!(s, "trimmed-slug");
    }

    #[tokio::test]
    async fn generate_slug_falls_back_on_invalid_chars() {
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec!["Bad Slug!!"]));
        let s = generate_slug(m, "the task", "graph").await.unwrap();
        assert!(s.starts_with("task-"));
        // Hex hash should follow.
        assert_eq!(s.len(), "task-".len() + 16);
    }

    #[tokio::test]
    async fn generate_slug_falls_back_on_empty_response() {
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec!["   "]));
        let s = generate_slug(m, "task", "graph").await.unwrap();
        assert!(s.starts_with("task-"));
    }

    #[test]
    fn is_valid_slug_accepts_typical_slugs() {
        assert!(is_valid_slug("my-skill"));
        assert!(is_valid_slug("plan-relocation-bjs-sha"));
        assert!(is_valid_slug("cargo-build-debug"));
        assert!(is_valid_slug("a-b-c-1-2-3"));
    }

    #[test]
    fn is_valid_slug_rejects_bad_inputs() {
        assert!(!is_valid_slug(""));
        assert!(!is_valid_slug("Bad-Slug"));
        assert!(!is_valid_slug("bad slug"));
        assert!(!is_valid_slug("bad_slug!"));
        assert!(!is_valid_slug("中文-slug"));
    }

    #[test]
    fn fallback_slug_is_deterministic() {
        let a = fallback_slug("the same task");
        let b = fallback_slug("the same task");
        let c = fallback_slug("a different task");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
