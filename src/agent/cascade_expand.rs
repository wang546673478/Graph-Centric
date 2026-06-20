//! CascadeExpander — recursive L0→L1→L2 graph expansion.
//!
//! When a Task node in the graph is too abstract to execute (no file paths,
//! no concrete actions in metadata), the CascadeExpander recursively
//! decomposes it into sub-nodes. Each level adds more detail:
//!
//! ```text
//! L0:  [T1: "改进消息样式"]                         ← abstract
//!        │
//!        │ expand_node(T1)
//!        ▼
//! L1:  [T1-A: "调整.msg margin"] → [T1-D: "完成"]   ← more concrete
//!        │
//!        │ expand_node(T1-A)
//!        ▼
//! L2:  [edit Transcript.vue: add margin-bottom:8px]  ← executable
//! ```
//!
//! A node is "executable" when its metadata contains:
//! - `files: [String]` — paths to modify
//! - `action: String` — what to do (edit/write)
//! - `validation: String` (optional) — how to verify
//!
//! Max expansion depth is 3 (L0→L1→L2). Sub-nodes are linked to their
//! parent via `Contains` edges, building a 3D multi-layer graph.

use crate::error::{HarnessError, Result};
use crate::graph::{Edge, Graph, Node, NodeId, NodeKind, RelationType};
use crate::model::{Message, Model, ModelRequest};
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Maximum expansion depth (L0 → L1 → L2 → stop).
const MAX_EXPAND_DEPTH: usize = 3;

/// A node is "concrete enough" to execute if it has these metadata fields.
fn is_executable(node: &Node) -> bool {
    node.metadata.get("files").map(|v| v.is_array()).unwrap_or(false)
        && node.metadata.get("action").map(|v| v.is_string()).unwrap_or(false)
}

/// Build a prompt that asks the model to expand one abstract node into
/// concrete sub-nodes.
fn expand_prompt(
    node: &Node,
    task_context: &str,
    depth: usize,
) -> String {
    let level_name = match depth {
        0 => "L1 (语义层 — 具体步骤)",
        1 => "L2 (实现层 — 文件路径+代码改动)",
        _ => "L2 (实现层 — 最小可执行单元)",
    };
    format!(
        r##"## 背景任务
{task_context}

## 需要展开的节点
- id: {id}
- summary: {summary}
- 当前层级: L{depth}

## 展开到: {level_name}

请将这个抽象节点拆解为 2-5 个更具体的子节点，形成一个 A（当前状态）→ 中间步骤 → D（完成状态）的子图。

## 输出格式
```json
{{
  "sub_nodes": [
    {{
      "id": "{id}-s1",
      "summary": "<具体做什么，1句话>",
      "metadata": {{
        "files": ["<文件路径>"],
        "action": "<edit|write|read>",
        "expected_change": "<预期的代码改动，1句话>",
        "validation": "<如何验证改动成功>"
      }}
    }}
  ],
  "edges": [
    {{"source": "{id}-s1", "target": "{id}-s2", "relation": "LeadsTo"}}
  ],
  "rationale": "<为什么这样拆解，1句话>"
}}
```

## 规则
1. 每个子节点 metadata 必须包含 files (数组)、action (字符串)
2. files 使用相对于项目根的路径，如 "webui/src/components/run/Transcript.vue"
3. expected_change 写具体的 CSS 属性或代码改动
4. 子节点之间用 LeadsTo 边连接(流程顺序);若有真正的依赖关系可用 DependsOn
5. 至少 2 个子节点，最多 5 个
6. 只输出 JSON，不要其他文字"##,
        id = node.id.as_str(),
        summary = node.summary,
        depth = depth,
    )
}

/// Recursively expand a graph, decomposing abstract Task nodes into
/// concrete sub-nodes. Returns the expanded graph with sub-graphs
/// linked via `Contains` edges.
pub async fn expand_graph(
    model: &dyn Model,
    mut graph: Graph,
    task: &str,
    max_depth: usize,
) -> Result<Graph> {
    let max_depth = max_depth.min(MAX_EXPAND_DEPTH);
    expand_recursive(model, &mut graph, task, 0, max_depth).await?;
    Ok(graph)
}

async fn expand_recursive(
    model: &dyn Model,
    graph: &mut Graph,
    task: &str,
    depth: usize,
    max_depth: usize,
) -> Result<()> {
    if depth >= max_depth {
        return Ok(());
    }

    // Collect nodes that need expansion (Task nodes that aren't executable).
    let to_expand: Vec<(NodeId, String)> = graph
        .nodes
        .values()
        .filter(|n| matches!(n.kind, NodeKind::Task) && !n.immutable && !is_executable(n))
        .map(|n| (n.id.clone(), n.summary.clone()))
        .collect();

    if to_expand.is_empty() {
        debug!(depth, "all nodes executable at this depth");
        return Ok(());
    }

    info!(
        count = to_expand.len(),
        depth,
        "cascade: expanding nodes"
    );

    for (node_id, _node_summary) in to_expand {
        // Skip if already expanded (has Contains children).
        let has_children = graph
            .outgoing(&node_id)
            .any(|e| e.relation == RelationType::Contains);
        if has_children {
            debug!(id = %node_id, "node already has Contains children, skipping");
            continue;
        }

        // Find the node to get its details.
        let node = match graph.get_node(&node_id) {
            Some(n) => n.clone(),
            None => continue,
        };

        let prompt = expand_prompt(&node, task, depth);
        let req = ModelRequest {
            messages: vec![Message::user(prompt)],
            tools: vec![],
            temperature: 0.1,
            max_tokens: Some(2048),
            stop: vec![],
        };

        match model.complete(req).await {
            Ok(resp) => {
                let content = resp.content.trim();
                match parse_expansion_response(content) {
                    Ok((sub_nodes, sub_edges, _rationale)) => {
                        let sub_count = sub_nodes.len();
                        if sub_count == 0 {
                            warn!(id = %node_id, "expansion returned 0 sub-nodes");
                            continue;
                        }
                        // Add sub-nodes to graph.
                        for mut sn in sub_nodes {
                            // Ensure Task kind.
                            sn.kind = NodeKind::Task;
                            // Add depth metadata.
                            sn.metadata
                                .entry("expansion_depth".to_string())
                                .or_insert_with(|| serde_json::Value::Number((depth + 1).into()));
                            let sn_id = sn.id.clone();
                            graph.add_node(sn);
                            // Link parent → child via Contains.
                            graph.add_edge(Edge::new(
                                node_id.clone(),
                                sn_id,
                                RelationType::Contains,
                                0.9,
                                format!("expanded from L{depth}"),
                            ))?;
                        }
                        // Add DependsOn sub-edges.
                        for se in sub_edges {
                            graph.add_edge(se)?;
                        }
                        info!(
                            id = %node_id,
                            sub_count,
                            depth,
                            "cascade: node expanded"
                        );
                    }
                    Err(e) => {
                        warn!(id = %node_id, error = %e, "cascade: failed to parse expansion, marking as leaf");
                        // Mark as leaf — can't expand further.
                        if let Some(node) = graph.get_node_mut(&node_id) {
                            node.metadata.insert("expansion_leaf".into(), true.into());
                        }
                    }
                }
            }
            Err(e) => {
                warn!(id = %node_id, error = %e, "cascade: model call failed for expansion");
            }
        }
    }

    // Recurse to next depth level.
    Box::pin(expand_recursive(model, graph, task, depth + 1, max_depth)).await
}

/// Parse the model's expansion response into sub-nodes and edges.
fn parse_expansion_response(
    content: &str,
) -> Result<(Vec<Node>, Vec<Edge>, String)> {
    // Use the same robust extraction as the proposer.
    let json_str = crate::agent::proposer::extract_json_block(content)
        .map_err(|_| HarnessError::domain("no JSON block in expansion response"))?;

    let parsed: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| HarnessError::domain(format!("expansion JSON parse: {e}")))?;

    let mut sub_nodes = Vec::new();
    if let Some(arr) = parsed.get("sub_nodes").and_then(|v| v.as_array()) {
        for item in arr {
            let id = item
                .get("id")
                .and_then(|v| v.as_str())
                .map(NodeId::from)
                .unwrap_or_else(|| NodeId::from("unnamed"));
            let summary = item
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let metadata: HashMap<String, serde_json::Value> = item
                .get("metadata")
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                })
                .unwrap_or_default();

            sub_nodes.push(Node {
                id,
                kind: NodeKind::Task,
                path: String::new(),
                summary,
                metadata,
                immutable: false,
                expanded: false,
            });
        }
    }

    let mut edges = Vec::new();
    if let Some(arr) = parsed.get("edges").and_then(|v| v.as_array()) {
        for item in arr {
            let source = item
                .get("source")
                .and_then(|v| v.as_str())
                .map(NodeId::from);
            let target = item
                .get("target")
                .and_then(|v| v.as_str())
                .map(NodeId::from);
            if let (Some(src), Some(tgt)) = (source, target) {
                edges.push(Edge::new(
                    src,
                    tgt,
                    RelationType::LeadsTo,
                    0.8,
                    "cascade expansion",
                ));
            }
        }
    }

    let rationale = parsed
        .get("rationale")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok((sub_nodes, edges, rationale))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_node_has_files_and_action() {
        let mut meta = HashMap::new();
        meta.insert("files".into(), serde_json::json!(["test.vue"]));
        meta.insert("action".into(), serde_json::json!("edit"));
        let node = Node {
            id: NodeId::from("t1"),
            kind: NodeKind::Task,
            path: String::new(),
            summary: "do something".into(),
            metadata: meta,
            immutable: false,
            expanded: false,
        };
        assert!(is_executable(&node));
    }

    #[test]
    fn abstract_node_is_not_executable() {
        let node = Node {
            id: NodeId::from("t1"),
            kind: NodeKind::Task,
            path: String::new(),
            summary: "improve styling".into(),
            metadata: HashMap::new(),
            immutable: false,
            expanded: false,
        };
        assert!(!is_executable(&node));
    }

    #[test]
    fn parse_expansion_works() {
        let resp = r#"```json
{
  "sub_nodes": [
    {"id": "t1-s1", "summary": "add margin", "metadata": {"files": ["a.vue"], "action": "edit", "expected_change": "add margin:8px"}},
    {"id": "t1-s2", "summary": "add line-height", "metadata": {"files": ["a.vue"], "action": "edit"}}
  ],
  "edges": [
    {"source": "t1-s1", "target": "t1-s2", "relation": "LeadsTo"}
  ],
  "rationale": "two-step style fix"
}
```"#;
        let (nodes, edges, rationale) = parse_expansion_response(resp).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].id.as_str(), "t1-s1");
        assert_eq!(edges.len(), 1);
        assert!(rationale.contains("two-step"));
    }
}
