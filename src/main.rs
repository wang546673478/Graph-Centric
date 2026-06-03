//! Binary entry point — for Phase 1 this is a smoke harness that proves the
//! library compiles, links, and that the core graph + scheduler primitives
//! can be exercised end-to-end without a model.

use graph_harness::{
    DagScheduler, Edge, Graph, Node, NodeId, RelationType, Result,
};
use tracing::info;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("graph-centric harness — phase 1 smoke");

    // ---- World graph (a tiny code structure) ----
    let mut world = Graph::new();
    world.add_node(Node::file("src/a.rs", "module A — request entry"));
    world.add_node(Node::file("src/b.rs", "module B — auth helpers"));
    world.add_node(Node::file("src/c.rs", "module C — db layer"));
    world.add_edge(Edge::new(
        "src/a.rs",
        "src/b.rs",
        RelationType::Imports,
        1.0,
        "static use",
    ))?;
    world.add_edge(Edge::new(
        "src/b.rs",
        "src/c.rs",
        RelationType::Calls,
        0.9,
        "direct call",
    ))?;
    info!(
        nodes = world.node_count(),
        edges = world.edge_count(),
        "world graph constructed"
    );

    let issues = world.find_inconsistencies();
    info!(issues = ?issues, "structural check");

    // ---- Task DAG, same Graph type, different RelationType convention ----
    //
    // Edge convention: dependent —DependsOn→ prerequisite.
    let task_graph = sample_task_dag()?;
    let schedule = DagScheduler::new().plan(&task_graph)?;
    info!(
        batches = schedule.depth(),
        tasks = schedule.task_count(),
        "task DAG scheduled"
    );
    for (i, batch) in schedule.batches.iter().enumerate() {
        let names: Vec<String> = batch.iter().map(NodeId::to_string).collect();
        info!(batch = i, tasks = ?names, "batch");
    }

    // ---- Local subgraph extraction around a focus node ----
    let sub = world.local_subgraph(&[NodeId::from("src/b.rs")], 1);
    info!(
        nodes = sub.node_count(),
        edges = sub.edge_count(),
        "local subgraph around src/b.rs at depth 1"
    );

    Ok(())
}

fn sample_task_dag() -> Result<Graph> {
    let mut g = Graph::new();
    for id in ["t1", "t2", "t3", "t4", "t5", "t6"] {
        g.add_node(Node::task(id, id));
    }
    // t3 depends on t1, t2
    g.add_edge(Edge::new("t3", "t1", RelationType::DependsOn, 1.0, ""))?;
    g.add_edge(Edge::new("t3", "t2", RelationType::DependsOn, 1.0, ""))?;
    // t4, t5 depend on t3
    g.add_edge(Edge::new("t4", "t3", RelationType::DependsOn, 1.0, ""))?;
    g.add_edge(Edge::new("t5", "t3", RelationType::DependsOn, 1.0, ""))?;
    // t6 depends on t4, t5
    g.add_edge(Edge::new("t6", "t4", RelationType::DependsOn, 1.0, ""))?;
    g.add_edge(Edge::new("t6", "t5", RelationType::DependsOn, 1.0, ""))?;
    Ok(g)
}
