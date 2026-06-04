//! Async fire-and-forget skill capture. Returns immediately; the actual
//! save (with two fast LLM calls) happens in a spawned tokio task.

use super::slug::generate_slug;
use super::storage::{LocalSkillStorage, SkillStorage};
use super::types::{Result, Skill, SkillError, SkillMeta, SkillRef};
use crate::graph::Graph;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::task::JoinHandle;

use crate::model::Model;

/// The function the caller (e.g., `bin/agent_a.rs`) invokes when a run
/// completes with `Reviewer` verdict `Pass`.
///
/// Returns a `JoinHandle<Result<SkillRef>>` immediately. The caller can
/// either discard the handle (fire-and-forget) or `await` it to learn
/// the resulting slug + trigger (e.g., the web gateway awaits so it
/// can emit a `SkillCaptured` SSE event). The spawned task runs:
/// 1. Generate slug (fast LLM call, ~1s)
/// 2. Generate trigger (fast LLM call, ~1-2s)
/// 3. Save to local skill storage
///
/// If any step fails, the skill is NOT saved; the `JoinHandle` resolves
/// to `Err(SkillError)` and a `warn!` is logged. No partial-save mode
/// in v1.
pub fn capture_skill(
    graph: Graph,
    review: serde_json::Value,
    task: String,
    task_id: Option<crate::graph::NodeId>,
    model: Arc<dyn Model>,
    storage: Arc<LocalSkillStorage>,
) -> JoinHandle<Result<SkillRef>> {
    tokio::spawn(async move {
        capture_inner(graph, review, task, task_id, model, storage).await
    })
}

async fn capture_inner(
    graph: Graph,
    review: serde_json::Value,
    task: String,
    task_id: Option<crate::graph::NodeId>,
    model: Arc<dyn Model>,
    storage: Arc<LocalSkillStorage>,
) -> Result<SkillRef> {
    let started = SystemTime::now();

    // 1. Slug
    let graph_summary = render_graph_summary(&graph);
    let slug = generate_slug(model.clone(), &task, &graph_summary).await?;

    // 2. Trigger
    let trigger = generate_trigger(model.clone(), &task, &graph_summary).await?;

    // 3. Metadata
    let meta = SkillMeta {
        created_at: iso8601_now(),
        task_id,
        model_used: model.name().to_string(),
        domain_tags: compute_domain_tags(&graph),
        l1_avg_confidence: l1_avg_confidence(&graph),
    };

    let skill = Skill {
        slug: slug.clone(),
        task,
        trigger: trigger.clone(),
        graph,
        review,
        meta,
    };

    // 4. Save
    storage.save(&skill)?;
    let elapsed = started.elapsed().map(|d| d.as_secs_f64()).unwrap_or(0.0);
    tracing::info!(
        skill = %slug,
        trigger = %trigger,
        l1_avg = skill.meta.l1_avg_confidence,
        elapsed_s = elapsed,
        "skill captured"
    );
    Ok(SkillRef { slug, trigger })
}

async fn generate_trigger(
    model: Arc<dyn Model>,
    task: &str,
    graph_summary: &str,
) -> Result<String> {
    let prompt = format!(
        "Task: {task}\n\n\
         Graph summary: {graph_summary}\n\n\
         Write ONE sentence starting with 'This skill applies when user asks about' \
         (or 'This skill applies when' for non-user-driven contexts). \
         The sentence should let a future agent decide when to consult this skill. \
         Output ONLY the sentence, no markdown, no preamble."
    );

    let request = crate::model::ModelRequest {
        messages: vec![crate::model::Message::user(prompt)],
        tools: Vec::new(),
        temperature: 0.3,
        max_tokens: Some(80),
        stop: Vec::new(),
    };

    let response = model.complete(request).await.map_err(|e| {
        SkillError::Model(format!("generate_trigger: {e}"))
    })?;

    let raw = response.content.trim().to_string();
    if raw.is_empty() {
        return Err(SkillError::Model("empty trigger response".into()));
    }
    Ok(raw)
}

fn render_graph_summary(graph: &Graph) -> String {
    // Cheap one-liner: node count + edge count + a few sample node ids.
    let n = graph.node_count();
    let e = graph.edge_count();
    let sample: Vec<String> = graph
        .nodes
        .keys()
        .take(5)
        .map(|id| id.to_string())
        .collect();
    format!("{n} nodes, {e} edges; sample: {}", sample.join(", "))
}

fn compute_domain_tags(graph: &Graph) -> Vec<String> {
    use crate::graph::NodeKind;
    use std::collections::BTreeSet;
    let mut tags: BTreeSet<String> = BTreeSet::new();
    for node in graph.nodes.values() {
        match node.kind {
            NodeKind::File | NodeKind::Function | NodeKind::Class | NodeKind::Module => {
                tags.insert("code".to_string());
            }
            NodeKind::Config => {
                tags.insert("infra".to_string());
            }
            NodeKind::Task => {
                tags.insert("business".to_string());
            }
            NodeKind::Other(_) => {
                // Skip — could be anything.
            }
        }
    }
    tags.into_iter().collect()
}

fn l1_avg_confidence(graph: &Graph) -> f64 {
    let confidences: Vec<f64> = graph.l1.iter().map(|(_, d)| d.confidence).collect();
    if confidences.is_empty() {
        0.0
    } else {
        confidences.iter().sum::<f64>() / confidences.len() as f64
    }
}

fn iso8601_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Graph, Node};
    use async_trait::async_trait;
    use crate::error::HarnessError;
    use crate::model::{FinishReason, Model, ModelRequest, ModelResponse, Usage};
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
        fn name(&self) -> &str { "mock" }
        async fn complete(
            &self,
            _req: ModelRequest,
        ) -> std::result::Result<ModelResponse, HarnessError> {
            let content = self.responses.lock().unwrap().pop()
                .unwrap_or_else(|| "default".to_string());
            Ok(ModelResponse {
                content,
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }
    }

    fn sample_graph_with_l1() -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::file("foo.rs", "foo"));
        g.add_node(Node::file("bar.rs", "bar"));
        // Set a sample L1 confidence.
        g.l1.set(
            "foo.rs".into(),
            crate::graph::L1Description::new("x", "y", "z", "w").with_confidence(0.8),
        );
        g
    }

    #[tokio::test]
    async fn capture_skill_resolves_with_skillref() {
        // The web gateway awaits the JoinHandle to emit a SkillCaptured
        // SSE event. Verify the resolved value actually contains the
        // model-generated slug + trigger.
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec![
            "happy-skill",
            "applies when user is happy",
        ]));

        let handle = capture_skill(
            sample_graph_with_l1(),
            serde_json::json!({}),
            "task".to_string(),
            None,
            m,
            storage,
        );
        let skill_ref = handle.await.unwrap().expect("capture should succeed");
        assert_eq!(skill_ref.slug, "happy-skill");
        assert_eq!(skill_ref.trigger, "applies when user is happy");
    }

    #[tokio::test]
    async fn capture_skill_writes_five_files() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec![
            "do-the-thing",
            "This skill applies when user asks to do the thing.",
        ]));

        let handle = capture_skill(
            sample_graph_with_l1(),
            serde_json::json!({"verdict": "pass"}),
            "do the thing".to_string(),
            None,
            m,
            storage.clone(),
        );
        let _ = handle.await.unwrap();

        // The captured skill lives in a subdirectory named after the slug.
        let skill_dir = dir.path().join("do-the-thing");
        assert!(skill_dir.join("task.md").exists());
        assert!(skill_dir.join("trigger.md").exists());
        assert!(skill_dir.join("graph.json").exists());
        assert!(skill_dir.join("review.json").exists());
        assert!(skill_dir.join("meta.json").exists());
    }

    #[tokio::test]
    async fn capture_skill_uses_slug_from_model() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec![
            "my-named-skill",
            "trigger text",
        ]));

        let handle = capture_skill(
            sample_graph_with_l1(),
            serde_json::json!({}),
            "task".to_string(),
            None,
            m,
            storage.clone(),
        );
        let _ = handle.await.unwrap();
        // After both LLM calls (slug first, then trigger), the skill
        // directory uses the LLM-provided slug.
        assert!(dir.path().join("my-named-skill").exists());
    }

    #[tokio::test]
    async fn capture_skill_does_not_save_on_llm_error() {
        struct FailingModel;
        #[async_trait]
        impl Model for FailingModel {
            fn name(&self) -> &str { "failing" }
            async fn complete(
                &self,
                _req: ModelRequest,
            ) -> std::result::Result<ModelResponse, HarnessError> {
                Err(HarnessError::model("simulated failure"))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let m: Arc<dyn Model> = Arc::new(FailingModel);

        let handle = capture_skill(
            sample_graph_with_l1(),
            serde_json::json!({}),
            "task".to_string(),
            None,
            m,
            storage,
        );
        let _ = handle.await.unwrap();
        // No subdirectory should have been created.
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(entries.is_empty(), "skill dir should be empty on LLM failure");
    }

    #[tokio::test]
    async fn capture_skill_includes_l1_confidence_in_meta() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec![
            "conf-test",
            "trigger",
        ]));

        let handle = capture_skill(
            sample_graph_with_l1(),  // one L1 entry with confidence 0.8
            serde_json::json!({}),
            "task".to_string(),
            None,
            m,
            storage.clone(),
        );
        let _ = handle.await.unwrap();
        let meta: SkillMeta = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("conf-test/meta.json")).unwrap(),
        ).unwrap();
        assert!((meta.l1_avg_confidence - 0.8).abs() < 1e-9);
    }

    #[tokio::test]
    async fn capture_skill_computes_domain_tags() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalSkillStorage::new(dir.path().to_path_buf()));
        let m: Arc<dyn Model> = Arc::new(MockModel::new(vec![
            "domain-test",
            "trigger",
        ]));

        let mut g = Graph::new();
        g.add_node(Node::file("a.rs", "a"));
        g.add_node(Node::task("t1", "do something"));
        let handle = capture_skill(
            g,
            serde_json::json!({}),
            "task".to_string(),
            None,
            m,
            storage.clone(),
        );
        let _ = handle.await.unwrap();
        let meta: SkillMeta = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("domain-test/meta.json")).unwrap(),
        ).unwrap();
        assert!(meta.domain_tags.contains(&"code".to_string()));
        assert!(meta.domain_tags.contains(&"business".to_string()));
    }
}
