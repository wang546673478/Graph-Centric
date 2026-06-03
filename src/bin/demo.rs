//! Phase 1 end-to-end demo.
//!
//! Drives the entire deterministic substrate on a real directory (defaulting
//! to this project's own `src/`):
//!
//! 1. Scan the directory → world graph
//! 2. Run structural validation
//! 3. Rank nodes by total connectivity
//! 4. Cut a local subgraph around the most-connected node
//! 5. Synthesize a tiny task DAG that "analyzes" the top files
//! 6. Schedule the DAG into wave-aligned batches
//! 7. Build a token-budgeted context bundle for one of the tasks
//! 8. Dump graph + context to `./demo_output/`
//!
//! Run:
//!
//! ```bash
//! cargo run --bin demo                  # defaults to ./src
//! cargo run --bin demo -- /some/path    # scan a different directory
//! ```

use graph_harness::context::{
    AssembledContext, ContextBuilder, FilesystemSources, render_local_graph,
};
use graph_harness::domain::Scanner;
use graph_harness::domain::code::CodeScanner;
use graph_harness::{
    DagScheduler, Edge, Graph, Node, NodeId, RelationType, Result,
};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let root = parse_args();
    let root_str = root
        .to_str()
        .expect("root path must be valid UTF-8")
        .to_string();
    info!(root = %root_str, "phase 1 demo starting");

    // [1/8] Scan -----------------------------------------------------------
    let scanner = CodeScanner::new();
    let graph = scanner.scan(&root_str).await?;
    info!(
        nodes = graph.node_count(),
        edges = graph.edge_count(),
        "[1/8] scanned source tree"
    );
    if graph.node_count() == 0 {
        warn!("no source files found — point demo at a directory containing code");
        return Ok(());
    }

    // [2/8] Structural validation ------------------------------------------
    let issues = graph.find_inconsistencies();
    info!(
        issues = issues.len(),
        "[2/8] structural validation"
    );
    for issue in issues.iter().take(5) {
        info!("  issue: {:?}", issue);
    }
    if issues.len() > 5 {
        info!("  …and {} more", issues.len() - 5);
    }

    // [3/8] Ranking --------------------------------------------------------
    let ranked = rank_by_connectivity(&graph);
    info!("[3/8] top connected nodes:");
    for (id, deg) in ranked.iter().take(8) {
        info!("  {:<48} {} edges", id.to_string(), deg);
    }
    let Some((top_id, _)) = ranked.first().cloned() else {
        warn!("no nodes to focus on, stopping");
        return Ok(());
    };

    // [4/8] Local subgraph -------------------------------------------------
    let sub = graph.local_subgraph(&[top_id.clone()], 2);
    info!(
        nodes = sub.node_count(),
        edges = sub.edge_count(),
        focus = %top_id,
        "[4/8] local subgraph (depth=2)"
    );
    let rendered = render_local_graph(&sub);
    print_block("local subgraph", &rendered, 30);

    // [5/8] Synthesize a task DAG that operates on the real graph ---------
    let task_dag = build_synthetic_task_dag(&ranked);
    info!(
        tasks = task_dag.node_count(),
        "[5/8] synthesized analysis-task DAG"
    );

    // [6/8] Schedule -------------------------------------------------------
    let schedule = DagScheduler::new()
        .with_max_batch_size(3)
        .plan(&task_dag)?;
    info!(
        batches = schedule.depth(),
        total = schedule.task_count(),
        "[6/8] scheduled into wave-aligned batches"
    );
    for (i, batch) in schedule.batches.iter().enumerate() {
        let names: Vec<String> = batch.iter().map(NodeId::to_string).collect();
        info!("  batch {}: {:?}", i, names);
    }

    // [7/8] Context for one task -------------------------------------------
    let loader = FilesystemSources::new(&root);
    let cb = ContextBuilder::new();
    let ctx = cb.build(
        "You are a code analyst exploring an unfamiliar codebase.",
        &format!(
            "Project at {}: {} files, {} relationships discovered.",
            root.display(),
            graph.node_count(),
            graph.edge_count()
        ),
        &format!(
            "Read {} and surrounding modules. Describe the module's role in two paragraphs.",
            top_id
        ),
        "(no prior results)",
        &graph,
        &[top_id.clone()],
        &loader,
    )?;
    report_context(&ctx, &top_id);

    // [8/8] Dump artifacts -------------------------------------------------
    let out_dir = PathBuf::from("./demo_output");
    fs::create_dir_all(&out_dir).map_err(|e| {
        graph_harness::HarnessError::context(format!("mkdir {}: {e}", out_dir.display()))
    })?;
    let graph_path = out_dir.join("graph.json");
    fs::write(&graph_path, graph.to_json()?).map_err(|e| {
        graph_harness::HarnessError::context(format!("write {}: {e}", graph_path.display()))
    })?;
    let ctx_path = out_dir.join("context.txt");
    fs::write(&ctx_path, &ctx.text).map_err(|e| {
        graph_harness::HarnessError::context(format!("write {}: {e}", ctx_path.display()))
    })?;
    let sub_path = out_dir.join("local_subgraph.txt");
    fs::write(&sub_path, render_local_graph(&sub)).map_err(|e| {
        graph_harness::HarnessError::context(format!("write {}: {e}", sub_path.display()))
    })?;
    info!(
        "[8/8] dumped {} ({} bytes), {} ({} bytes), {} ({} bytes)",
        graph_path.display(),
        size(&graph_path),
        ctx_path.display(),
        size(&ctx_path),
        sub_path.display(),
        size(&sub_path),
    );

    info!("demo complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_args() -> PathBuf {
    std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./src"))
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
}

fn rank_by_connectivity(graph: &Graph) -> Vec<(NodeId, usize)> {
    let mut scored: Vec<(NodeId, usize)> = graph
        .iter_nodes()
        .map(|n| {
            let deg = graph.outgoing(&n.id).count() + graph.incoming(&n.id).count();
            (n.id.clone(), deg)
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.as_str().cmp(b.0.as_str())));
    scored
}

/// Build a tiny analysis DAG: one "analyze" task per top-3 hub file, plus a
/// final "synthesize" task that depends on all of them.
fn build_synthetic_task_dag(ranked: &[(NodeId, usize)]) -> Graph {
    let mut g = Graph::new();
    let synthesize = "t_synthesize";
    g.add_node(Node::task(
        synthesize,
        "Synthesize findings across analyzed files",
    ));

    for (i, (file_id, _)) in ranked.iter().take(3).enumerate() {
        let task_id = format!("t_analyze_{i}");
        let node = Node::task(
            &task_id,
            format!("Analyze {} and its immediate neighbors", file_id),
        )
        .with_metadata("focus", serde_json::Value::from(file_id.as_str()));
        g.add_node(node);
        // synthesize DependsOn t_analyze_i
        let _ = g.add_edge(Edge::new(
            synthesize,
            task_id,
            RelationType::DependsOn,
            1.0,
            "must finish before synthesis",
        ));
    }
    g
}

fn print_block(title: &str, body: &str, max_lines: usize) {
    info!("--- {} ---", title);
    for line in body.lines().take(max_lines) {
        info!("  {}", line);
    }
    let total = body.lines().count();
    if total > max_lines {
        info!("  …({} more lines)", total - max_lines);
    }
}

fn report_context(ctx: &AssembledContext, focus: &NodeId) {
    info!(
        "[7/8] context for analyzing {}: {} tokens total",
        focus, ctx.used_tokens
    );
    let mut sections: Vec<(&&'static str, &usize)> = ctx.section_tokens.iter().collect();
    sections.sort_by(|a, b| b.1.cmp(a.1));
    for (label, tokens) in sections {
        info!("  section {:<14} {} tokens", label, tokens);
    }
}

fn size(p: &Path) -> u64 {
    fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}
