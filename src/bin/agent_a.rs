//! `agent-a` — domain-agnostic main agent driving the iterative graph loop.
//!
//! Demo A's purpose: prove the full Phase 2.5 + Phase 3 stack works end-to-end
//! on a task that has nothing to do with code. The agent loads model config
//! from `.env`, wires up Proposer + Verifier + LocalRepairer + L1Enricher +
//! Decomposer + Dispatcher + GraphLoop, then runs the loop while handling
//! user I/O through stdin.
//!
//! ### Usage
//!
//! ```bash
//! # Make sure .env is filled in.
//! cargo run --bin agent_a -- "help me plan a relocation from Beijing to Shanghai"
//!
//! # Or omit the arg to be prompted:
//! cargo run --bin agent_a
//! ```
//!
//! ### Outputs (written to `./demo_output/`)
//!
//! - `agent_a_graph.json`        — final relationship graph (L0 + L1)
//! - `agent_a_transcript.txt`    — full conversation history
//! - `agent_a_task_outcome.json` — sub-agent results from the Task phase
//!                                  (present only when Phase 3 actually ran)
//!
//! ### Model tier assignment
//!
//! | Component       | Tier | Reason                                          |
//! |-----------------|------|-------------------------------------------------|
//! | GraphProposer   | fast | called every Graph-phase turn; volume matters   |
//! | Verifier        | fast | sampled + self-check; volume matters            |
//! | L1Enricher      | deep | semantic quality matters more than throughput   |
//! | LocalRepairer   | deep | each repair should land on first try            |
//! | **Decomposer**  | deep | task decomposition quality is high-leverage     |
//! | **SubAgent**    | fast | one per sub-task; high volume                   |

use graph_harness::agent::{
    BashCheckValidator, Decomposer, Dispatcher, GraphLoop, GraphLoopConfig, GraphProposer,
    L1Enricher, LocalRepairer, LoopState, PostExecutionValidator, Reviewer, SubAgent, Verifier,
};
use graph_harness::context::FilesystemSources;
use graph_harness::model::ModelConfig;
use graph_harness::skills::{
    capture::capture_skill, storage::LocalSkillStorage, CompositeSkillStorage, RepoSkillStorage,
    SkillStorage,
};
use graph_harness::tools::{BashTool, ReadOnly, ToolRegistry};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() {
    init_tracing();

    if let Err(e) = run().await {
        error!("agent_a: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // ---- Config ----
    let cfg = ModelConfig::load()?;
    info!(
        base_url = %cfg.base_url,
        fast = %cfg.fast,
        deep = %cfg.deep,
        "loaded ModelConfig"
    );

    // ---- Task ----
    let task = parse_task_arg().unwrap_or_else(|| {
        prompt_line("Describe the task for the agent (one paragraph is fine):\n> ")
            .expect("failed to read task from stdin")
    });
    if task.trim().is_empty() {
        eprintln!("Empty task; nothing to do.");
        return Ok(());
    }
    info!(task = %task, "starting agent_a");

    // ---- Tools ----
    // Read-only Bash so the agent can inspect files when the task happens
    // to involve them. Most domain-agnostic tasks won't call it; code-ish
    // tasks can use it to read source/configs.
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(BashTool::new()));
    let tools = Arc::new(registry);

    // ---- Models (tiered) ----
    let proposer_model = cfg.fast_model();
    let verifier_model = cfg.fast_model();
    let enricher_model = cfg.deep_model();
    let repairer_model = cfg.deep_model();
    let decomposer_model = cfg.deep_model();
    let subagent_model = cfg.fast_model();
    let reviewer_model = cfg.deep_model();

    // ---- L2 loader ----
    // FilesystemSources for L2 lookups. For abstract task nodes (no files),
    // the loader will error gracefully and both L1Enricher and SubAgent's
    // ContextBuilder fall back to their inference paths (Phase 2.5 work).
    let cwd = std::env::current_dir()?;
    let loader = Arc::new(FilesystemSources::new(cwd.clone()));

    // ---- Skill storage (Phase: skill capture & reuse) ----
    // Two roots: a `LocalSkillStorage` for new captures (default install at
    // `~/.local/share/graph-centric/skills/`, with a tempdir fallback when
    // `$HOME` isn't set) and a `RepoSkillStorage` at `<cwd>/skills/` for
    // approved/curated skills. The Proposer sees the composite; `capture_skill`
    // uses the local root directly because the spec is "new saves always go
    // to local; promote to repo by filesystem". The composite's local and
    // the capture's local share the same on-disk root (different in-process
    // instances, since `LocalSkillStorage` is not `Clone`), but only the
    // capture path writes — the composite is read-only by design
    // (`CompositeSkillStorage::save` returns an error).
    let local_storage: Arc<LocalSkillStorage> = Arc::new(
        LocalSkillStorage::default_install().unwrap_or_else(|| {
            LocalSkillStorage::new(std::env::temp_dir().join("graph-centric-skills-fallback"))
        }),
    );
    let composite_local_root = local_storage
        .local_root()
        .expect("LocalSkillStorage always reports its local root");
    let repo_storage = RepoSkillStorage::new(cwd.join("skills"));
    let skill_storage: Arc<dyn graph_harness::skills::SkillStorage> =
        Arc::new(CompositeSkillStorage::new(
            Some(LocalSkillStorage::new(composite_local_root)),
            repo_storage,
        ));

    // ---- Build the loop ----
    let proposer = GraphProposer::new(
        proposer_model.clone(),
        tools.clone(),
        Some(skill_storage.clone()),
    );
    let verifier = Verifier::with_model(verifier_model).with_loader(loader.clone());
    let enricher = L1Enricher::new(enricher_model, loader.clone());
    let repairer = LocalRepairer::new(repairer_model).with_l1_enricher(enricher.clone());

    // Phase 3 — model-driven task decomposition + concurrent sub-agent execution.
    // Phase 4: each SubAgent now has access to the same tool registry +
    // ReadOnly policy as the main agent, with a max_steps cap on its
    // tool-calling loop. This lets sub-agents `bash cat <file>` to fetch
    // real L2 when their assigned slice involves files.
    let decomposer = Decomposer::new(decomposer_model);
    let subagent = Arc::new(
        SubAgent::new(subagent_model)
            .with_tools(tools.clone())
            .with_policy(Arc::new(ReadOnly))
            .with_tool_cwd(cwd.clone())
            .with_tool_output_cap(6_000)
            .with_max_steps(6),
    );
    // 2 subagents in parallel = 1 main + 2 subagents total
    // (per [[project-concurrency-limits]]). Main runs single-threaded
    // as the orchestrator; the pool below caps subagent fan-out.
    let dispatcher = Dispatcher::new(subagent).with_max_concurrent(2);

    // Phase 4 — Reviewer: deterministic backstops (graph consistency +
    // sub-agent success + last_verification) plus LLM-as-judge that flags
    // graph / task / scope root causes. Failed reviews with
    // graph/scope root_cause bubble back as LoopState::GraphInvalid for
    // the caller to repair.
    let reviewer = Reviewer::with_model(reviewer_model);

    // Phase 4 — PostExecutionValidator: runs `cargo check` between Task and
    // Review phases. If it fails with a graph-error pattern in the output
    // ("cannot find function", "unresolved import", etc.) the loop bubbles
    // GraphInvalid { source: PostExecutionValidation } — bypassing the
    // expensive LLM-as-judge for the common compile-failure case.
    //
    // Demo A sub-agents are ReadOnly so they can't actually break the
    // code; this validator is here to prove the wiring + surface the
    // verdict in the Done branch. For demos that allow writes, the same
    // validator catches sub-agent regressions in real time.
    let validator: Arc<dyn PostExecutionValidator> =
        Arc::new(BashCheckValidator::cargo_check_for(&cwd));

    // Keep a separate handle on the repairer for the auto-repair loop
    // below. Both copies share Arcs to the model + enricher under the hood,
    // so cloning is cheap.
    let auto_repairer = repairer.clone();

    let loop_cfg = GraphLoopConfig {
        // Per [[project-concurrency-limits]] the main agent and each
        // sub-agent get a 180-turn budget (no internal cap below that
        // — it's a ceiling, not a target). The total concurrent runs
        // cap is 3 across main + subagents; with the main loop running
        // single-threaded, that means the dispatcher pool below is
        // sized to 2.
        max_rounds: 180,
        max_repair_rounds: 3,
        tool_cwd: cwd.clone(),
        tool_output_cap: 8_000,
        tool_policy: Arc::new(ReadOnly),
    };

    let mut gl = GraphLoop::new(task.clone(), proposer, verifier, Some(repairer), tools, loop_cfg)
        .with_l1_enricher(enricher)
        .with_decomposer(decomposer)
        .with_dispatcher(dispatcher)
        .with_subagent_loader(loader)
        .with_validator(validator)
        .with_reviewer(reviewer);

    println!("\n══════════════════════════════════════════════════════");
    println!(" Graph-Centric Agent — Demo A (Phase 3 + Phase 4 enabled)");
    println!(" Task: {}", truncate_for_display(&task, 80));
    println!("══════════════════════════════════════════════════════\n");

    // Auto-repair budget. Each `LoopState::GraphInvalid` consumes one
    // attempt: we call `LocalRepairer::repair_from_error` for each error,
    // apply the patches, and `resume_with_repaired_graph`. Beyond this
    // many cycles we surface to the user and exit.
    const MAX_AUTO_REPAIR_CYCLES: usize = 3;
    let mut auto_repair_cycles: usize = 0;

    // ---- Drive the loop ----
    loop {
        let state = gl.step().await;
        match state {
            LoopState::Running => {
                println!(
                    "  [round {}] {} nodes / {} edges / {} L1",
                    gl.round,
                    gl.graph.node_count(),
                    gl.graph.edge_count(),
                    gl.graph.l1.len()
                );
            }
            LoopState::Paused { question, rationale } => {
                println!("\n┌─ AGENT ASKS ────────────────────────────────────────");
                if !rationale.trim().is_empty() {
                    println!("│ (rationale: {})", truncate_for_display(&rationale, 100));
                }
                println!("│ {}", question);
                println!("└─────────────────────────────────────────────────────");
                let answer = prompt_line("YOU > ")?;
                if answer.trim() == ":quit" || answer.trim() == ":q" {
                    println!("\nAborted by user.");
                    break;
                }
                gl.resume(answer);
            }
            LoopState::GraphInvalid {
                source,
                errors,
                snapshot,
            } => {
                auto_repair_cycles += 1;
                println!("\n┌─ GRAPH INVALID ─────────────────────────────────────");
                println!("│ source: {:?}", source);
                println!(
                    "│ snapshot: {} nodes / {} edges",
                    snapshot.node_count(),
                    snapshot.edge_count()
                );
                for (i, err) in errors.iter().take(8).enumerate() {
                    println!("│ [{}] {}: {}", i, err.kind_label(), err.detail());
                }
                if errors.len() > 8 {
                    println!("│ …and {} more", errors.len() - 8);
                }
                println!(
                    "│ auto-repair cycle {}/{}",
                    auto_repair_cycles, MAX_AUTO_REPAIR_CYCLES
                );
                println!("└─────────────────────────────────────────────────────");

                if auto_repair_cycles > MAX_AUTO_REPAIR_CYCLES {
                    warn!(
                        "auto-repair budget exhausted after {} cycles; exiting",
                        MAX_AUTO_REPAIR_CYCLES
                    );
                    dump_outputs(&gl, &task)?;
                    std::process::exit(2);
                }

                // Run the LocalRepairer on each error in turn, applying
                // patches to a working copy of the snapshot. Per principle
                // #3 (local repair, never bulk): one error at a time, each
                // producing a small scoped patch.
                let mut repaired = snapshot.clone();
                let mut applied = 0usize;
                let mut failed = 0usize;
                for err in &errors {
                    match auto_repairer.repair_from_error(&repaired, err, &task).await {
                        Ok(patch) => match repaired.apply_patch(patch) {
                            Ok(()) => {
                                applied += 1;
                            }
                            Err(e) => {
                                failed += 1;
                                warn!(error = %e, "auto-repair patch failed to apply");
                            }
                        },
                        Err(e) => {
                            failed += 1;
                            warn!(error = %e, kind = err.kind_label(), "auto-repair errored");
                        }
                    }
                }
                println!(
                    "  Auto-repair: {} patch(es) applied, {} repair attempt(s) failed",
                    applied, failed
                );

                if applied == 0 {
                    // Nothing got fixed; further cycles unlikely to help.
                    warn!("auto-repair produced no applied patches; exiting");
                    dump_outputs(&gl, &task)?;
                    std::process::exit(2);
                }

                gl.resume_with_repaired_graph(repaired);
            }
            LoopState::TaskFailed { failures } => {
                println!(
                    "\n┌─ TASK PHASE: {} SUB-AGENT FAILURE(S) ──────────────",
                    failures.len()
                );
                for f in failures.iter().take(8) {
                    println!("│ • {}: {}", f.task_id, truncate_for_display(&f.error, 120));
                }
                if failures.len() > 8 {
                    println!("│ …and {} more", failures.len() - 8);
                }
                println!("└─────────────────────────────────────────────────────");
                dump_outputs(&gl, &task)?;
                std::process::exit(2);
            }
            LoopState::Done(result) => {
                println!("\n══════════════════════════════════════════════════════");
                println!(" DONE in {} rounds", result.rounds);
                println!(
                    " Graph: {} nodes / {} edges / {} L1 entries",
                    result.graph.node_count(),
                    result.graph.edge_count(),
                    result.graph.l1.len(),
                );
                if let Some(v) = &result.last_verification {
                    println!(" Verification: {}", v.rationale);
                }
                if let Some(outcome) = &result.task_outcome {
                    println!(
                        " Task phase: {} sub-task(s), all_succeeded={}, sub-agent wall_ms={}, tokens={}",
                        outcome.results.len(),
                        outcome.all_succeeded,
                        outcome.total_subagent_ms,
                        outcome.total_tokens
                    );
                    if outcome.results.is_empty() {
                        println!(" (decomposer produced no sub-tasks — task likely too simple to decompose)");
                    } else {
                        println!(" Sub-task summary:");
                        for r in outcome.results.iter().take(8) {
                            let preview = r.output.lines().next().unwrap_or("").trim();
                            println!(
                                "   - {} ({} tokens, {}ms): {}",
                                r.task_id,
                                r.tokens_used,
                                r.duration_ms,
                                truncate_for_display(preview, 90)
                            );
                        }
                        if outcome.results.len() > 8 {
                            println!("   …and {} more", outcome.results.len() - 8);
                        }
                    }
                } else {
                    println!(" Task phase: (skipped — Phase 3 components not all configured)");
                }
                if let Some(review) = &result.review_result {
                    println!(
                        " Review verdict: passed={}",
                        review.passed
                    );
                    for c in &review.deterministic_checks {
                        let mark = if c.passed { "✓" } else { "✗" };
                        println!("   {} {} — {}", mark, c.name, truncate_for_display(&c.details, 80));
                    }
                    if let Some(j) = &review.judge_verdict {
                        let mark = if j.passed { "✓" } else { "✗" };
                        println!(
                            "   {} judge (confidence {:.2}): {}{}",
                            mark,
                            j.confidence,
                            j.detail,
                            j.root_cause
                                .as_ref()
                                .map(|rc| format!(" [root_cause={:?}]", rc))
                                .unwrap_or_default()
                        );
                    }

                    // Phase: skill capture & reuse. When the review passed,
                    // fire `capture_skill` in the background — the returned
                    // `JoinHandle` is dropped (fire-and-forget). The
                    // `local_storage` Arc is shared with the in-loop
                    // composite, but only the capture path actually writes
                    // to it (the composite's `save` is intentionally
                    // disabled in v1).
                    if review.passed {
                        let review_json = serde_json::to_value(review)
                            .unwrap_or(serde_json::Value::Null);
                        let handle = capture_skill(
                            gl.graph.clone(),
                            review_json,
                            task.clone(),
                            None,
                            proposer_model.clone(),
                            local_storage.clone(),
                        );
                        // Fire-and-forget: drop the handle. The capture
                        // task continues to run in the background.
                        drop(handle);
                        println!(" Skill capture kicked off in the background.");
                    } else {
                        info!("review did not pass; skipping skill capture");
                    }
                } else {
                    println!(" Review: (skipped — no Reviewer configured)");
                }
                println!("══════════════════════════════════════════════════════\n");
                dump_outputs(&gl, &task)?;
                break;
            }
            LoopState::Error(msg) => {
                eprintln!("\nLoop error: {msg}");
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

fn dump_outputs(gl: &GraphLoop, task: &str) -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from("./demo_output");
    fs::create_dir_all(&out_dir)?;
    let graph_path = out_dir.join("agent_a_graph.json");
    let txt_path = out_dir.join("agent_a_transcript.txt");

    fs::write(&graph_path, gl.graph.to_json()?)?;

    let mut t = String::new();
    t.push_str(&format!("# Task\n{task}\n\n# Transcript\n\n"));
    t.push_str(&gl.conversation.transcript());
    if let Some(v) = &gl.last_verification {
        t.push_str(&format!("\n# Final verification\n{}\n", v.rationale));
    }
    fs::write(&txt_path, t)?;

    // Phase 3 outputs (only when the Task phase actually ran)
    let outcome_path = if let Some(outcome) = &gl.task_outcome {
        let path = out_dir.join("agent_a_task_outcome.json");
        fs::write(&path, serde_json::to_string_pretty(outcome)?)?;
        Some(path)
    } else {
        None
    };

    // Phase 4 outputs (only when the Reviewer ran)
    let review_path = if let Some(review) = &gl.review_result {
        let path = out_dir.join("agent_a_review.json");
        fs::write(&path, serde_json::to_string_pretty(review)?)?;
        Some(path)
    } else {
        None
    };

    println!(
        "Wrote {} ({} B) and {} ({} B){}{}",
        graph_path.display(),
        fs::metadata(&graph_path).map(|m| m.len()).unwrap_or(0),
        txt_path.display(),
        fs::metadata(&txt_path).map(|m| m.len()).unwrap_or(0),
        match &outcome_path {
            Some(p) => format!(
                " and {} ({} B)",
                p.display(),
                fs::metadata(p).map(|m| m.len()).unwrap_or(0)
            ),
            None => String::new(),
        },
        match &review_path {
            Some(p) => format!(
                " and {} ({} B)",
                p.display(),
                fs::metadata(p).map(|m| m.len()).unwrap_or(0)
            ),
            None => String::new(),
        }
    );
    Ok(())
}

fn parse_task_arg() -> Option<String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        None
    } else {
        Some(args.join(" "))
    }
}

fn prompt_line(prompt: &str) -> io::Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    let line = line.trim_end_matches(['\n', '\r']).to_string();
    Ok(line)
}

fn truncate_for_display(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
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
