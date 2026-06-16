//! Skill → Task Graph compiler.
//!
//! Converts a captured `Skill`'s graph (L0 nodes + edges) into a task DAG
//! suitable for the `Decomposer` / `Dispatcher`. Every L0 node becomes a
//! `NodeKind::Task` node; every edge becomes `RelationType::DependsOn`.
//!
//! The output graph can be fed directly to the scheduler.

use crate::graph::{Edge, Graph, Node, NodeId, NodeKind, RelationType};
use crate::skills::types::Skill;

/// Compile a Skill's graph into a task graph.
///
/// Mapping rules:
/// - L0 nodes → Task nodes with prefixed id (`skill:<slug>:<node_id>`)
/// - L0 edges → DependsOn edges
/// - L1 descriptions → task summaries (fallback: L0 summary)
/// - Skill trigger → metadata on each task node
pub fn compile_skill_to_task_graph(skill: &Skill) -> Graph {
    let mut graph = Graph::new();
    let prefix = format!("skill:{}:", skill.slug);

    for (node_id, node) in &skill.graph.nodes {
        let task_id = NodeId::from(format!("{prefix}{node_id}"));
        let description = skill
            .graph
            .l1
            .get(node_id)
            .map(|l1| l1.render_oneline())
            .unwrap_or_else(|| node.summary.clone());

        let mut task_node = Node::task(task_id.as_str(), description);
        task_node
            .metadata
            .insert("skill_slug".into(), serde_json::json!(skill.slug));
        task_node
            .metadata
            .insert("skill_trigger".into(), serde_json::json!(skill.trigger));
        task_node
            .metadata
            .insert("skill_node_id".into(), serde_json::json!(node_id.as_str()));
        graph.add_node(task_node);
    }

    for edge in &skill.graph.edges {
        let source = NodeId::from(format!("{prefix}{}", edge.source));
        let target = NodeId::from(format!("{prefix}{}", edge.target));
        if graph.contains_node(&source) && graph.contains_node(&target) {
            let _ = graph.add_edge(Edge::new(
                source,
                target,
                RelationType::DependsOn,
                edge.confidence,
                edge.evidence.clone(),
            ));
        }
    }

    graph.rebuild_indices();
    graph
}

/// Compile and return as a rendered string suitable for a prompt.
pub fn render_compiled_task_graph(skill: &Skill) -> String {
    let g = compile_skill_to_task_graph(skill);
    let mut s = String::new();
    s.push_str(&format!(
        "Compiled skill `{}` ({} nodes, {} edges):\n",
        skill.slug,
        g.node_count(),
        g.edge_count()
    ));
    for node in g.nodes.values() {
        s.push_str(&format!(
            "  - {} (Task) summary={:?} tags={}\n",
            node.id.as_str(),
            node.summary,
            node
                .metadata
                .get("skill_trigger")
                .and_then(|v| v.as_str())
                .unwrap_or("")
        ));
    }
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::types::{Skill, SkillMeta};
    use serde_json::json;

    fn make_test_skill() -> Skill {
        let mut g = Graph::new();
        g.add_node(Node::task("a", "research APIs"));
        g.add_node(Node::task("b", "write code"));
        g.add_edge(Edge::new("a", "b", RelationType::DependsOn, 0.8, "needs research"))
            .unwrap();

        Skill {
            slug: "research-then-write".into(),
            task: "research and write code".into(),
            trigger: "when user asks to research then implement".into(),
            graph: g,
            review: json!({"passed": true}),
            meta: SkillMeta {
                created_at: "2026-01-01".into(),
                task_id: None,
                model_used: "test".into(),
                domain_tags: vec!["code".into()],
                l1_avg_confidence: 0.7,
            },
        }
    }

    #[test]
    fn compiles_nodes_to_task_kind() {
        let skill = make_test_skill();
        let g = compile_skill_to_task_graph(&skill);
        assert_eq!(g.node_count(), 2);
        for node in g.nodes.values() {
            assert!(
                matches!(node.kind, NodeKind::Task),
                "node {} should be Task, got {:?}",
                node.id.as_str(),
                node.kind
            );
        }
    }

    #[test]
    fn prefixes_node_ids_with_skill_slug() {
        let skill = make_test_skill();
        let g = compile_skill_to_task_graph(&skill);
        assert!(g.contains_node(&NodeId::from("skill:research-then-write:a")));
        assert!(g.contains_node(&NodeId::from("skill:research-then-write:b")));
    }

    #[test]
    fn maps_edges_to_depends_on() {
        let skill = make_test_skill();
        let g = compile_skill_to_task_graph(&skill);
        assert_eq!(g.edge_count(), 1);
        let edge = &g.edges[0];
        assert_eq!(edge.relation, RelationType::DependsOn);
        assert_eq!(edge.source.as_str(), "skill:research-then-write:a");
        assert_eq!(edge.target.as_str(), "skill:research-then-write:b");
    }

    #[test]
    fn adds_skill_metadata() {
        let skill = make_test_skill();
        let g = compile_skill_to_task_graph(&skill);
        let node = g
            .nodes
            .get(&NodeId::from("skill:research-then-write:a"))
            .unwrap();
        assert_eq!(
            node.metadata.get("skill_slug").unwrap().as_str().unwrap(),
            "research-then-write"
        );
    }

    #[test]
    fn empty_skill_produces_empty_graph() {
        let mut skill = make_test_skill();
        skill.graph = Graph::new();
        let g = compile_skill_to_task_graph(&skill);
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn dag_is_schedulable() {
        let skill = make_test_skill();
        let g = compile_skill_to_task_graph(&skill);
        let plan = crate::scheduler::DagScheduler::new().plan(&g);
        assert!(plan.is_ok(), "compiled DAG should be schedulable");
    }
}
