//! CascadeBacktracker — verify predecessors when a downstream node changes.
//!
//! When a sub-agent reports that node K cannot be executed and the main
//! agent re-plans K → K', this component walks inbound edges from K' to
//! verify that each predecessor's design and output still satisfy K''s
//! new requirements. Verification stops at the immutable anchor node.

use crate::context::SourceLoader;
use crate::error::Result;
use crate::graph::{Edge, Graph, Node, NodeId, RelationType};
use crate::model::{Message, Model, ModelRequest};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The verdict for a single predecessor verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PredecessorVerdict {
    /// Design and output are both valid for the new successor.
    Preserved,
    /// Design is invalid — needs re-planning.
    NeedsRepair(String),
    /// Design is valid but the output is stale — needs re-execution.
    NeedsReexecution(String),
}

/// The aggregated result of a cascade backtracking pass.
#[derive(Debug, Clone)]
pub struct CascadeResult {
    /// Nodes whose design + output are still valid.
    pub preserved: Vec<NodeId>,
    /// Nodes whose design needs re-planning (triggers recursive backtrack).
    pub needs_repair: Vec<NodeId>,
    /// Nodes whose design is ok but output needs refresh.
    pub needs_reexec: Vec<NodeId>,
}

/// One step in the cascade, emitted as an event for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct CascadeStep {
    pub changed_node: String,
    pub predecessor: String,
    pub depth: usize,
    pub verdict: String,
    pub rationale: String,
}

// ---------------------------------------------------------------------------
// CascadeBacktracker
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct CascadeBacktracker {
    /// Model used for verification decisions (typically deep tier).
    pub model: Arc<dyn Model>,
    /// Safety cap on how many hops to backtrack from the changed node.
    pub max_depth: usize,
    /// Temperature for verification calls (low — want deterministic judgment).
    pub temperature: f64,
    /// Optional callback for emitting cascade steps to the UI.
    /// When set, each verification step is pushed through this channel.
    pub step_sink: Option<tokio::sync::mpsc::UnboundedSender<CascadeStep>>,
}

impl CascadeBacktracker {
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self {
            model,
            max_depth: 50,
            temperature: 0.0,
            step_sink: None,
        }
    }

    pub fn with_step_sink(mut self, sink: tokio::sync::mpsc::UnboundedSender<CascadeStep>) -> Self {
        self.step_sink = Some(sink);
        self
    }

    /// Entry point. Called after node K is fixed/replaced by K'.
    /// Walks all inbound edges from K', verifies each predecessor,
    /// recurses on failures, stops at anchor.
    pub async fn backtrack_from(
        &self,
        changed_node: &NodeId,
        graph: &Graph,
        task: &str,
        l2_loader: &dyn SourceLoader,
    ) -> Result<CascadeResult> {
        let mut result = CascadeResult {
            preserved: Vec::new(),
            needs_repair: Vec::new(),
            needs_reexec: Vec::new(),
        };

        let changed = match graph.nodes.get(changed_node) {
            Some(n) => n,
            None => return Ok(result),
        };

        self.backtrack_recursive(changed, graph, task, l2_loader, 0, &mut result)
            .await?;

        Ok(result)
    }

    async fn backtrack_recursive(
        &self,
        successor: &Node,
        graph: &Graph,
        task: &str,
        l2_loader: &dyn SourceLoader,
        depth: usize,
        result: &mut CascadeResult,
    ) -> Result<()> {
        if depth >= self.max_depth {
            warn!(depth, "cascade backtracking hit max_depth; stopping");
            return Ok(());
        }

        let preds = dependency_predecessors_of(graph, &successor.id);

        for (_, pred) in &preds {
            // Stop at anchor — it's immutable.
            if pred.immutable {
                debug!(anchor = %pred.id, "cascade reached anchor; stopping branch");
                continue;
            }

            // Skip if already classified in this pass.
            if result.preserved.contains(&pred.id)
                || result.needs_repair.contains(&pred.id)
                || result.needs_reexec.contains(&pred.id)
            {
                continue;
            }

            let verdict = self
                .verify_predecessor(pred, successor, graph, task, l2_loader)
                .await?;

            let step = CascadeStep {
                changed_node: successor.id.to_string(),
                predecessor: pred.id.to_string(),
                depth,
                verdict: match &verdict {
                    PredecessorVerdict::Preserved => "preserved".into(),
                    PredecessorVerdict::NeedsRepair(_) => "needs_repair".into(),
                    PredecessorVerdict::NeedsReexecution(_) => "needs_reexec".into(),
                },
                rationale: match &verdict {
                    PredecessorVerdict::Preserved => "design and output still valid".into(),
                    PredecessorVerdict::NeedsRepair(r) => r.clone(),
                    PredecessorVerdict::NeedsReexecution(r) => r.clone(),
                },
            };
            info!(step = ?step, "cascade step");
            if let Some(sink) = &self.step_sink {
                let _ = sink.send(step);
            }

            match verdict {
                PredecessorVerdict::Preserved => {
                    result.preserved.push(pred.id.clone());
                }
                PredecessorVerdict::NeedsRepair(_) => {
                    result.needs_repair.push(pred.id.clone());
                    // Recurse: this predecessor may have its own predecessors.
                    Box::pin(self.backtrack_recursive(
                        pred,
                        graph,
                        task,
                        l2_loader,
                        depth + 1,
                        result,
                    ))
                    .await?;
                }
                PredecessorVerdict::NeedsReexecution(_) => {
                    result.needs_reexec.push(pred.id.clone());
                    // Design correct, output stale. No need to recurse.
                }
            }
        }

        Ok(())
    }

    /// Ask the model: does predecessor P still satisfy successor S's input
    /// requirements after S was redesigned?
    pub async fn verify_predecessor(
        &self,
        predecessor: &Node,
        successor: &Node,
        graph: &Graph,
        task: &str,
        l2_loader: &dyn SourceLoader,
    ) -> Result<PredecessorVerdict> {
        let pred_l1 = graph.l1.get(&predecessor.id);
        let succ_l1 = graph.l1.get(&successor.id);

        let pred_l2 = l2_loader.load(&predecessor.id).unwrap_or_default();
        let succ_l2 = l2_loader.load(&successor.id).unwrap_or_default();
        let pred_l2_snippet: String = pred_l2.chars().take(2000).collect();
        let succ_l2_snippet: String = succ_l2.chars().take(2000).collect();

        let prompt = format!(
            r#"You are verifying a relationship graph after a design change.

## Context
Task: {task}

## The Changed Node (successor)
Node ID: {succ_id}
Kind: {succ_kind}
New L1 Design: {succ_l1}
New L2 Content (first 2000 chars): {succ_l2}

## The Predecessor You Must Verify
Node ID: {pred_id}
Kind: {pred_kind}
Current L1 Design: {pred_l1}
Current Output (L2, first 2000 chars): {pred_l2}

## Question
The successor node was just redesigned. Does the predecessor's design
and output STILL satisfy the successor's input requirements?

- If YES (both design and output are still valid): respond PRESERVED.
- If the DESIGN is wrong for the new successor (not just the output):
  respond NEEDS_REPAIR and explain why.
- If the DESIGN is correct but the OUTPUT is stale/wrong: respond
  NEEDS_REEXECUTION and explain what needs refreshing.

Respond with JSON:
{{"verdict": "PRESERVED|NEEDS_REPAIR|NEEDS_REEXECUTION", "rationale": "..."}}"#,
            succ_id = successor.id.as_str(),
            succ_kind = successor.kind.as_wire(),
            succ_l1 = succ_l1
                .map(|l| l.responsibility.as_str())
                .unwrap_or("(none)"),
            succ_l2 = succ_l2_snippet,
            pred_id = predecessor.id.as_str(),
            pred_kind = predecessor.kind.as_wire(),
            pred_l1 = pred_l1
                .map(|l| l.responsibility.as_str())
                .unwrap_or("(none)"),
            pred_l2 = pred_l2_snippet,
        );

        let req = ModelRequest {
            messages: vec![Message::system(prompt)],
            tools: vec![],
            temperature: self.temperature,
            max_tokens: Some(512),
            stop: vec![],
        };

        let resp = self.model.complete(req).await?;
        let content = resp.content.trim();

        let verdict: serde_json::Value = serde_json::from_str(content).unwrap_or(serde_json::json!({
            "verdict": "PRESERVED",
            "rationale": "parse failed, assuming preserved"
        }));

        let v = verdict["verdict"].as_str().unwrap_or("PRESERVED");
        let rationale = verdict["rationale"].as_str().unwrap_or("").to_string();

        match v {
            "NEEDS_REPAIR" => Ok(PredecessorVerdict::NeedsRepair(rationale)),
            "NEEDS_REEXECUTION" => Ok(PredecessorVerdict::NeedsReexecution(rationale)),
            _ => Ok(PredecessorVerdict::Preserved),
        }
    }
}

fn dependency_predecessors_of<'a>(
    graph: &'a Graph,
    node: &NodeId,
) -> Vec<(&'a Edge, &'a Node)> {
    // Upstream = nodes whose structural edge points INTO `node` (they feed
    // it). With the start→deliverable flow, an edge source→target means
    // source flows to target, so `node`'s upstream are edges with
    // target == node. Walk LeadsTo/DependsOn/Contains (structural) edges.
    graph
        .edges
        .iter()
        .filter(|e| e.relation.is_structural() && e.target == *node)
        .filter_map(|e| graph.nodes.get(&e.source).map(|n| (e, n)))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, RelationType};
    use crate::model::{FinishReason, ModelResponse, Usage};
    use async_trait::async_trait;

    /// A model that always returns "PRESERVED" — useful for structural tests.
    struct AlwaysPreservedModel;
    #[async_trait]
    impl Model for AlwaysPreservedModel {
        fn name(&self) -> &str {
            "always_preserved"
        }
        async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse> {
            Ok(ModelResponse {
                content: r#"{"verdict":"PRESERVED","rationale":"test"}"#.into(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                reasoning_content: None,
                usage: Usage::default(),
            })
        }
    }

    struct StubLoader;
    impl SourceLoader for StubLoader {
        fn load(&self, _node_id: &crate::graph::NodeId) -> Result<String> {
            Ok("// stub content".into())
        }
    }

    fn make_test_graph() -> Graph {
        // Anchor A → B → C (leaf)
        let mut g = Graph::new();
        let mut a = Node::task("a", "anchor");
        a.immutable = true;
        g.add_node(a);
        g.add_node(Node::task("b", "middle"));
        g.add_node(Node::task("c", "leaf"));
        // Forward flow: source → target means source feeds target.
        // a feeds b feeds c, so edges are a→b and b→c.
        g.add_edge(Edge::new(
            "a", "b", RelationType::DependsOn, 1.0, "",
        ))
        .unwrap();
        g.add_edge(Edge::new(
            "b", "c", RelationType::DependsOn, 1.0, "",
        ))
        .unwrap();
        g
    }

    #[tokio::test]
    async fn backtrack_from_leaf_preserves_b_and_stops_at_anchor() {
        let g = make_test_graph();
        let model: Arc<dyn Model> = Arc::new(AlwaysPreservedModel);
        let cascade = CascadeBacktracker::new(model);
        let loader = StubLoader;

        let result = cascade
            .backtrack_from(
                &NodeId::from("c"),
                &g,
                "test task",
                &loader,
            )
            .await
            .unwrap();

        assert!(result.preserved.contains(&NodeId::from("b")));
        // Anchor a should NOT appear in any result vector (it's immutable, skipped).
        assert!(!result.preserved.contains(&NodeId::from("a")));
        assert!(!result.needs_repair.contains(&NodeId::from("a")));
        assert!(result.needs_repair.is_empty());
        assert!(result.needs_reexec.is_empty());
    }

    #[tokio::test]
    async fn backtrack_from_nonexistent_node_returns_empty() {
        let g = make_test_graph();
        let model: Arc<dyn Model> = Arc::new(AlwaysPreservedModel);
        let cascade = CascadeBacktracker::new(model);
        let loader = StubLoader;

        let result = cascade
            .backtrack_from(
                &NodeId::from("nonexistent"),
                &g,
                "test",
                &loader,
            )
            .await
            .unwrap();

        assert!(result.preserved.is_empty());
        assert!(result.needs_repair.is_empty());
        assert!(result.needs_reexec.is_empty());
    }
}
