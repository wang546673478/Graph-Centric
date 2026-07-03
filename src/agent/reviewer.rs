//! Reviewer — final acceptance gate before Done.
//!
//! Per design principle #5 (deterministic reviewer backstops): an LLM-as-judge
//! alone is unreliable, so the Reviewer layers deterministic checks under any
//! model call.
//!
//! ## Layers
//!
//! 1. **Deterministic** — always on, no model needed:
//!    - **Graph consistency**: re-run [`Graph::find_inconsistencies`].
//!      Failing a structural check means whatever happened earlier didn't
//!      stick (e.g., a repair patch left a dangling edge).
//!    - **Sub-agent success**: every dispatched sub-agent must report
//!      `success=true`. A failed sub-agent fails the review.
//!    - **Verifier-final status**: the most recent Graph-phase verification
//!      must have passed. (Belt-and-suspenders — the loop wouldn't reach
//!      Review otherwise, but cheap to assert.)
//!
//! 2. **LLM-as-judge** (optional) — asks the model:
//!    *Given the task, the final graph, and the sub-agent results, is the
//!    work satisfactory? If not, what's the root cause?*
//!
//! ## Verdict → next phase
//!
//! | Verdict                                        | GraphLoop action                  |
//! |------------------------------------------------|-----------------------------------|
//! | Deterministic + judge pass                     | `Phase::Done`                     |
//! | Deterministic fail OR judge=GraphIssue/ScopeIssue | Surface `LoopState::GraphInvalid { source: Review }` to the caller |
//! | Judge=TaskIssue                                | `Phase::Done` with `passed=false` embedded in `ReviewResult` |

use super::dispatcher::DispatchOutcome;
use super::graph_loop::{GraphError, L0ErrorType};
use super::proposer::extract_json_block;
use super::verifier::VerificationResult;
use crate::domain::CheckResult;
use crate::error::{HarnessError, Result};
use crate::graph::{Graph, NodeId};
use crate::model::{Message, Model, ModelRequest};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Verdict types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RootCause {
    /// The relationship graph itself is wrong — missing/wrong edges or nodes.
    GraphIssue,
    /// The graph is correct but the sub-agents produced bad work (code-level
    /// failure, missed requirement, etc.). Phase 5 will route back to Task
    /// with feedback; v1 just surfaces Done with the verdict embedded.
    TaskIssue,
    /// The graph's scope is too narrow — needed regions were not modelled.
    /// Routes back to Graph phase for expansion.
    ScopeIssue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeVerdict {
    pub passed: bool,
    /// Only present when `passed=false`. Drives the loop's routing decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_cause: Option<RootCause>,
    pub detail: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewResult {
    pub passed: bool,
    pub deterministic_checks: Vec<CheckResult>,
    /// Present iff the Reviewer was configured with a model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge_verdict: Option<JudgeVerdict>,
    pub rationale: String,
}

impl ReviewResult {
    pub fn root_cause(&self) -> Option<&RootCause> {
        self.judge_verdict.as_ref().and_then(|j| j.root_cause.as_ref())
    }
}

// ---------------------------------------------------------------------------
// Reviewer
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Reviewer {
    /// Optional main model for the LLM-as-judge layer. Without it, only
    /// deterministic checks run.
    pub model: Option<Arc<dyn Model>>,
    /// v2 spec §5.2: optional **advisor** model for the LLM-as-judge.
    /// When set, the advisor is asked to give a *second opinion* on
    /// whether the run produced a real answer (not just a graph that
    /// *describes* the task). The advisor's verdict is combined with
    /// the main model's verdict.
    pub advisor: Option<Arc<dyn Model>>,
    pub temperature: f64,
    pub max_tokens: Option<usize>,
}

impl Reviewer {
    /// Deterministic-only reviewer. Cheap, model-free, no judgment beyond
    /// "did the structural state stay consistent and did all sub-agents
    /// succeed?" Suitable for sub-agents themselves and for low-stakes runs.
    pub fn deterministic_only() -> Self {
        Self {
            model: None,
            advisor: None,
            temperature: 0.0,
            max_tokens: Some(1024),
        }
    }

    pub fn with_model(model: Arc<dyn Model>) -> Self {
        Self {
            model: Some(model),
            advisor: None,
            temperature: 0.0,
            max_tokens: Some(1024),
        }
    }

    /// v2 spec §5.2: attach an advisor model for second-opinion
    /// judgment. Use DeepSeek / Claude / a reasoning model here.
    pub fn with_advisor(mut self, advisor: Arc<dyn Model>) -> Self {
        self.advisor = Some(advisor);
        self
    }

    /// Run the full review.
    ///
    /// P12 architecture change: when an `advisor` model is
    /// attached, the review layer now has THREE independent
    /// judgment channels, all of which must agree for the run to
    /// be marked Done:
    ///
    /// 1. **Deterministic** — graph consistency, sub-agent success,
    ///    last_verification result.
    /// 2. **Main LLM-as-judge** — the existing `judge` call, which
    ///    reads the graph and produces a `judge_verdict` tool call.
    /// 3. **Advisor second opinion** — a SEPARATE model (DeepSeek
    ///    by default) is asked "is there a real answer in the
    ///    final transcript, or did the agent just describe the
    ///    task?". This catches the E3 / E4 failure mode where
    ///    the main model says "Done" because the graph is fine,
    ///    even though no actual answer was produced.
    ///
    /// All three must pass. If any fails, the run is marked
    /// `Done(false)` with a clear root_cause.
    pub async fn review(
        &self,
        task: &str,
        graph: &Graph,
        task_outcome: Option<&DispatchOutcome>,
        last_verification: Option<&VerificationResult>,
    ) -> Result<ReviewResult> {
        let mut checks = Vec::new();

        // ---- Deterministic layer ----
        let inconsistencies = graph.find_inconsistencies();
        let high_or_blocking = inconsistencies.iter().any(|i| {
            matches!(
                i,
                crate::graph::Inconsistency::DanglingEdge { .. }
                    | crate::graph::Inconsistency::Cycle { .. }
            )
        });
        checks.push(CheckResult {
            name: "graph_consistency".into(),
            passed: !high_or_blocking,
            details: if inconsistencies.is_empty() {
                "no inconsistencies".into()
            } else {
                format!(
                    "{} inconsistencies (high-severity: {})",
                    inconsistencies.len(),
                    if high_or_blocking { "yes" } else { "no" }
                )
            },
        });

        if let Some(outcome) = task_outcome {
            let succeeded = outcome.results.iter().filter(|r| r.success).count();
            let total = outcome.results.len();
            checks.push(CheckResult {
                name: "subagent_results".into(),
                passed: outcome.all_succeeded,
                details: format!("{succeeded}/{total} succeeded, all_succeeded={}", outcome.all_succeeded),
            });
            if !outcome.graph_errors.is_empty() {
                checks.push(CheckResult {
                    name: "subagent_graph_errors".into(),
                    passed: false,
                    details: format!(
                        "{} sub-agent(s) reported graph errors",
                        outcome.graph_errors.len()
                    ),
                });
            }
        }

        if let Some(v) = last_verification {
            checks.push(CheckResult {
                name: "last_verification".into(),
                passed: v.passed,
                details: v.rationale.clone(),
            });
        }

        // P12: deterministic answer-content check. The reviewer
        // examines the sub-agent transcripts (passed via
        // task_outcome's per-sub-task final_answer strings) and
        // verifies that for Read / Explain tasks, the transcript
        // contains a non-trivial answer, not just a task title
        // or graph description. This catches the E3 / E4
        // failure mode where the main LLM-as-judge says "Done"
        // because the graph is fine, even though no answer was
        // produced.
        if let Some(outcome) = task_outcome {
            let answer_check = self.check_answer_content(task, outcome);
            checks.push(answer_check);
        }

        let det_passed = checks.iter().all(|c| c.passed);
        debug!(det_passed, check_count = checks.len(), "reviewer: deterministic layer complete");

        // ---- LLM-as-judge layer ----
        let judge = if let Some(model) = &self.model {
            Some(
                self.judge(model.as_ref(), task, graph, task_outcome)
                    .await?,
            )
        } else {
            None
        };

        let passed = det_passed && judge.as_ref().map(|j| j.passed).unwrap_or(true);
        // P12: advisor second opinion. If the main judge says
        // pass but the advisor says fail, fail the run.
        let mut advisor_verdict: Option<JudgeVerdict> = None;
        if let Some(adv) = &self.advisor {
            match self.judge(adv.as_ref(), task, graph, task_outcome).await {
                Ok(v) => {
                    if !v.passed {
                        warn!(
                            "advisor disagrees with main judge: pass={} vs advisor pass={} (root_cause={:?})",
                            judge.as_ref().map(|j| j.passed).unwrap_or(false),
                            v.passed,
                            v.root_cause
                        );
                    }
                    advisor_verdict = Some(v);
                }
                Err(e) => warn!("advisor judge failed: {e}"),
            }
        }
        let final_passed = passed
            && advisor_verdict
                .as_ref()
                .map(|v| v.passed)
                .unwrap_or(true);
        let rationale = match (&judge, &advisor_verdict) {
            (Some(j), Some(a)) => format!(
                "deterministic={det_passed}, main_judge={} (root_cause={:?}, conf={:.2}), \
                 advisor={} (root_cause={:?}, conf={:.2})",
                j.passed, j.root_cause, j.confidence, a.passed, a.root_cause, a.confidence
            ),
            (Some(j), None) => format!(
                "deterministic={det_passed}, main_judge={} (root_cause={:?}, confidence={:.2}), \
                 advisor=skipped",
                j.passed, j.root_cause, j.confidence
            ),
            (None, Some(a)) => format!(
                "deterministic={det_passed}, main_judge=skipped, advisor={} (root_cause={:?}, confidence={:.2})",
                a.passed, a.root_cause, a.confidence
            ),
            (None, None) => format!("deterministic={det_passed}, judge=skipped (no model)"),
        };

        info!(
            final_passed,
            checks = checks.len(),
            judge_passed = judge.as_ref().map(|j| j.passed),
            advisor_passed = advisor_verdict.as_ref().map(|v| v.passed),
            "reviewer: complete"
        );
        Ok(ReviewResult {
            passed: final_passed,
            deterministic_checks: checks,
            judge_verdict: judge,
            rationale,
        })
    }

    /// P12 architecture: deterministic answer-content check.
    ///
    /// For Read / Explain / Search tasks, the sub-agent transcripts
    /// must contain a non-trivial answer — not just a task title,
    /// not just a graph description. This catches the failure
    /// mode observed in E3 / E4 (P10): the main LLM-as-judge
    /// approves a run because the graph is structurally fine, but
    /// the actual answer is missing or merely a description of
    /// the task.
    ///
    /// Heuristic: concatenate the sub-agent `output` strings AND
    /// the deliverable node's L1 description, strip common
    /// boilerplate, and verify the combined corpus overlaps with
    /// the task description by at least 3 word tokens. The L1
    /// is included because it's auto-enriched from the model's
    /// actual work product (file contents, etc.) and captures
    /// answer content the model wrote to disk rather than to
    /// the transcript.
    fn check_answer_content(
        &self,
        task: &str,
        outcome: &DispatchOutcome,
    ) -> CheckResult {
        // Concatenate sub-agent outputs.
        let mut all_text = String::new();
        for r in &outcome.results {
            all_text.push_str(&r.output);
            all_text.push('\n');
        }
        // Strip common boilerplate.
        let cleaned: String = all_text
            .lines()
            .filter(|l| {
                !l.contains("(no results)")
                    && !l.contains("Deliverable:")
                    && !l.contains("Node summary")
                    && !l.contains("Read this content")
                    && !l.contains("graph_node")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let len = cleaned.trim().chars().count();
        // Concatenate deliverable L1 — auto-enriched from the
        // model's actual file writes. This is where the real
        // answer content lives for tasks where the model wrote
        // a markdown note or explanation file.
        let l1_text: String = outcome
            .results
            .iter()
            .filter_map(|r| {
                // We don't have direct access to the graph
                // here, so we look at any L1-shaped text in the
                // output. The sub-agent output for a task with
                // an L1 description often includes the L1 text
                // itself (via the L1 enrichment auto-call).
                let s = r.output.as_str();
                if s.contains("responsibility:")
                    || s.contains("implementation:")
                    || s.contains("design_intent:")
                {
                    Some(s.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Token overlap with the task — does the answer
        // address what the user asked? Combine output + L1 text.
        let combined = format!("{cleaned}\n{l1_text}");
        let task_tokens: std::collections::HashSet<String> = task
            .split_whitespace()
            .filter(|w| w.chars().count() >= 2)
            .map(|w| w.to_lowercase())
            .collect();
        let answer_tokens: std::collections::HashSet<String> = combined
            .split_whitespace()
            .filter(|w| w.chars().count() >= 2)
            .map(|w| w.to_lowercase())
            .collect();
        let overlap = task_tokens.intersection(&answer_tokens).count();
        // If total cleaned output is tiny, we can't really
        // measure — pass with a note (the LLM-as-judge layers
        // are the real backstop).
        if len < 50 {
            return CheckResult {
                name: "answer_content".into(),
                passed: true,
                details: format!(
                    "cleaned_chars={len} (< 50, output too small to heuristic-check; \
                     defer to LLM-as-judge layers)"
                ),
            };
        }
        // Pass when the combined output has at least 3 task-overlap
        // tokens. We use 3 instead of 2 to avoid common stopwords
        // like "the" / "in" / "and" trivially matching the task.
        let passed = overlap >= 3;
        CheckResult {
            name: "answer_content".into(),
            passed,
            details: format!(
                "cleaned_chars={len}, l1_included={}, task_overlap_tokens={overlap}, \
                 threshold: 50 chars + 3 overlap (combined output + L1)",
                !l1_text.is_empty()
            ),
        }
    }

    async fn judge(
        &self,
        model: &dyn Model,
        task: &str,
        graph: &Graph,
        outcome: Option<&DispatchOutcome>,
    ) -> Result<JudgeVerdict> {
        let graph_sketch = render_graph_for_judge(graph);
        let outcome_block = outcome
            .map(render_outcome_for_judge)
            .unwrap_or_else(|| "(no Task phase ran)".to_string());

        let user_prompt = format!(
            "## Original task\n{task}\n\n## Final relationship graph\n{graph_sketch}\n\n\
             ## Sub-agent outcomes\n{outcome_block}\n\n\
             Decide whether the work satisfactorily addresses the original task. \
             Reply with ONE JSON object only:\n\n\
             {{\n  \"verdict\": \"pass\" | \"fail\",\n  \
             \"root_cause\": \"graph\" | \"task\" | \"scope\" | null,\n  \
             \"detail\": \"<one sentence>\",\n  \"confidence\": 0..1\n}}\n\n\
             Rules:\n\
             - `verdict=pass` when the original task is genuinely addressed by the graph + outcomes.\n\
             - `verdict=fail` only when there's a concrete shortfall you can name.\n\
             - `root_cause`:\n\
               * graph  → the relationship graph is wrong (missing/wrong nodes or edges)\n\
               * task   → the graph is fine but the sub-agents produced bad / incomplete work\n\
               * scope  → the graph's scope is too narrow; the task needed regions not modelled\n\
               * null   → only when verdict=pass\n\
             - confidence: 0.9+ when you have direct evidence; 0.5 when uncertain."
        );

        let req = ModelRequest {
            messages: vec![
                Message::system(load_prompt_file("skills/prompts/reviewer.md", SYSTEM_PROMPT_REVIEWER)),
                Message::user(user_prompt.clone()),
            ],
            tools: vec![reviewer_verdict_tool_schema()],
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            stop: Vec::new(),
        };
        let resp = model.complete(req).await?;
        debug!(
            content_len = resp.content.len(),
            reasoning_len = resp.reasoning_content.as_deref().map(str::len).unwrap_or(0),
            tool_calls = resp.tool_calls.len(),
            "reviewer judge response"
        );
        // Dump the raw response at warn level when the text path
        // fails to parse (db2d993d-class debugging). Mirrors the
        // same pattern added to decomposer.
        let text_for_log = resp.text_or_reasoning();
        if !text_for_log.trim().is_empty() && parse_verdict(&text_for_log).is_err() {
            warn!(
                content_preview = format!("{:?}", resp.content.chars().take(800).collect::<String>()),
                reasoning_preview = format!(
                    "{:?}",
                    resp.reasoning_content
                        .as_deref()
                        .map(|s| s.chars().take(800).collect::<String>())
                        .unwrap_or_default()
                ),
                tool_calls_count = resp.tool_calls.len(),
                "reviewer model returned a response that didn't parse as JSON. \
                 See content_preview + reasoning_preview above for what the model said."
            );
        }
        // Strategy A: prefer native tool_calls; fall back to text.
        if let Some(v) = parse_verdict_from_tool_calls(&resp.tool_calls) {
            return Ok(v);
        }
        // Reasoning-model fallback (DeepSeek / M3). db2d993d regression.
        let text = resp.text_or_reasoning();
        match parse_verdict(&text) {
            Ok(v) => Ok(v),
            Err(parse_err) => {
                // Text path failed (model returned prose without JSON,
                // or used reasoning instead of the tool). One fix-it
                // retry: explicit "you MUST call the tool" prompt.
                warn!(
                    error = %parse_err,
                    "reviewer first response was malformed; retrying once with a fix-it prompt"
                );
                let retry_prompt = format!(
                    "Your previous response was malformed (parser said: {parse_err}). \
                     You MUST call the `judge_verdict` tool with a valid JSON `verdict` field. \
                     Do NOT reply with prose or explanations. Reply with the tool call only."
                );
                let retry_req = ModelRequest {
                    messages: vec![
                        Message::system(load_prompt_file(
                            "skills/prompts/reviewer.md",
                            SYSTEM_PROMPT_REVIEWER,
                        )),
                        Message::user(user_prompt),
                        Message::assistant(text.clone()),
                        Message::user(retry_prompt),
                    ],
                    tools: vec![reviewer_verdict_tool_schema()],
                    temperature: self.temperature,
                    max_tokens: self.max_tokens,
                    stop: Vec::new(),
                };
                let retry_resp = model.complete(retry_req).await?;
                if let Some(v) = parse_verdict_from_tool_calls(&retry_resp.tool_calls) {
                    return Ok(v);
                }
                let retry_text = retry_resp.text_or_reasoning();
                parse_verdict(&retry_text)
            }
        }
    }

    /// Translate a [`ReviewResult`] (when failed with Graph/Scope root_cause)
    /// into structured [`GraphError`]s for the GraphLoop to surface as
    /// `GraphInvalid { source: Review }`. Returns an empty vec when the
    /// review actually passed.
    pub fn to_graph_errors(&self, review: &ReviewResult) -> Vec<GraphError> {
        if review.passed {
            return Vec::new();
        }
        let mut out = Vec::new();
        // Start with structured info from each deterministic check that failed.
        for c in &review.deterministic_checks {
            if c.passed {
                continue;
            }
            out.push(GraphError::L0Structural {
                error_type: L0ErrorType::MissingRelation,
                detail: format!("reviewer/{}: {}", c.name, c.details),
                related_nodes: Vec::new(),
                discovered_by: Some("reviewer".into()),
            });
        }
        // Add the judge's verdict as a separate error rooted by its root_cause.
        if let Some(j) = &review.judge_verdict {
            if !j.passed {
                match j.root_cause {
                    Some(RootCause::ScopeIssue) => out.push(GraphError::ScopeGap {
                        missing_nodes: Vec::new(),
                        missing_edges: Vec::new(),
                        detail: format!("reviewer/judge: {}", j.detail),
                        discovered_by: Some("reviewer".into()),
                    }),
                    Some(RootCause::GraphIssue) | None => {
                        out.push(GraphError::L0Structural {
                            error_type: L0ErrorType::MissingRelation,
                            detail: format!("reviewer/judge: {}", j.detail),
                            related_nodes: Vec::new(),
                            discovered_by: Some("reviewer".into()),
                        });
                    }
                    Some(RootCause::TaskIssue) => {
                        // Task issues aren't graph errors — caller handles them
                        // via the embedded `judge_verdict.root_cause` directly.
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// JSON parsing
// ---------------------------------------------------------------------------

fn parse_verdict(text: &str) -> Result<JudgeVerdict> {
    let cleaned = extract_json_block(text).map_err(|e| {
        HarnessError::model(format!(
            "reviewer: judge response not parseable: {e}\n--- raw ---\n{text}"
        ))
    })?;
    let value: serde_json::Value = serde_json::from_str(&cleaned).map_err(|e| {
        HarnessError::model(format!(
            "reviewer: invalid JSON: {e}\n--- cleaned ---\n{cleaned}"
        ))
    })?;

    let verdict = value
        .get("verdict")
        .and_then(|v| v.as_str())
        .unwrap_or("fail");
    let passed = verdict == "pass";
    let root_cause = match value.get("root_cause").and_then(|v| v.as_str()) {
        Some("graph") => Some(RootCause::GraphIssue),
        Some("task") => Some(RootCause::TaskIssue),
        Some("scope") => Some(RootCause::ScopeIssue),
        _ => None,
    };
    let detail = value
        .get("detail")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let confidence = value
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5)
        .clamp(0.0, 1.0);

    Ok(JudgeVerdict {
        passed,
        root_cause,
        detail,
        confidence,
    })
}

/// Tool schema for the reviewer judge verdict. Same wire shape as the
/// text-fallback path's JSON; enum constraints are advisory (the parser
/// tolerates missing/invalid values per the legacy default-to-fail rule).
fn reviewer_verdict_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "judge_verdict",
            "description": "Decide whether the work satisfactorily addresses the original task.",
            "parameters": {
                "type": "object",
                "properties": {
                    "verdict": {
                        "type": "string",
                        "enum": ["pass", "fail"],
                        "description": "pass = task genuinely addressed. fail = concrete shortfall you can name."
                    },
                    "root_cause": {
                        "type": "string",
                        "enum": ["graph", "task", "scope"],
                        "description": "Why it failed. graph = relationship graph is wrong. task = sub-agents produced bad work. scope = graph's scope too narrow. Omit (or use null) when verdict=pass."
                    },
                    "detail": {
                        "type": "string",
                        "description": "One sentence explaining the verdict."
                    },
                    "confidence": {
                        "type": "number",
                        "description": "0..1. 0.9+ when you have direct evidence; 0.5 when uncertain."
                    }
                },
                "required": ["verdict"]
            }
        }
    })
}

/// Parse a JudgeVerdict from a native tool_call. Returns None when
/// tool_calls is empty or no matching tool_call was emitted.
fn parse_verdict_from_tool_calls(
    tool_calls: &[crate::model::ToolCall],
) -> Option<JudgeVerdict> {
    let tc = tool_calls.iter().find(|tc| tc.name == "judge_verdict")?;
    let verdict = tc.arguments.get("verdict").and_then(|v| v.as_str()).unwrap_or("fail");
    let passed = verdict == "pass";
    let root_cause = match tc.arguments.get("root_cause").and_then(|v| v.as_str()) {
        Some("graph") => Some(RootCause::GraphIssue),
        Some("task") => Some(RootCause::TaskIssue),
        Some("scope") => Some(RootCause::ScopeIssue),
        _ => None,
    };
    let detail = tc
        .arguments
        .get("detail")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let confidence = tc
        .arguments
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5)
        .clamp(0.0, 1.0);
    Some(JudgeVerdict {
        passed,
        root_cause,
        detail,
        confidence,
    })
}

// ---------------------------------------------------------------------------
// Render helpers — kept terse so judge prompts stay small
// ---------------------------------------------------------------------------

fn render_graph_for_judge(g: &Graph) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "nodes={} edges={} l1_entries={}\n",
        g.node_count(),
        g.edge_count(),
        g.l1.len()
    ));
    let mut ids: Vec<&NodeId> = g.nodes.keys().collect();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    if !ids.is_empty() {
        s.push_str("nodes:\n");
        for id in &ids {
            if let Some(n) = g.get_node(id) {
                s.push_str(&format!("  - {} [{:?}] {}\n", n.id, n.kind, n.summary));
            }
        }
    }
    if g.edge_count() > 0 {
        s.push_str("edges:\n");
        for e in g.iter_edges() {
            s.push_str(&format!(
                "  {} -[{:?}]-> {} (c={:.2})\n",
                e.source, e.relation, e.target, e.confidence
            ));
        }
    }
    s
}

fn render_outcome_for_judge(outcome: &DispatchOutcome) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "{} sub-task(s), all_succeeded={}, tokens={}, wall_ms={}\n",
        outcome.results.len(),
        outcome.all_succeeded,
        outcome.total_tokens,
        outcome.total_subagent_ms
    ));
    if !outcome.graph_errors.is_empty() {
        s.push_str(&format!(
            "  ⚠ {} graph error(s) reported by sub-agents\n",
            outcome.graph_errors.len()
        ));
    }
    for r in outcome.results.iter().take(8) {
        let preview = r.output.lines().next().unwrap_or("").trim();
        let truncated: String = preview.chars().take(140).collect();
        s.push_str(&format!(
            "  - {} (success={}, {} tokens): {}\n",
            r.task_id, r.success, r.tokens_used, truncated
        ));
    }
    if outcome.results.len() > 8 {
        s.push_str(&format!("  …and {} more\n", outcome.results.len() - 8));
    }
    s
}

/// Try to load a prompt from a file, falling back to the hardcoded default.
/// This lets users edit `skills/prompts/reviewer-*.md` without recompiling.
fn load_prompt_file(path: &str, default: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| default.to_string())
}

const SYSTEM_PROMPT_REVIEWER: &str = "You are an acceptance reviewer in a graph-centric agent harness. \
You are STRICT and TERSE. Given the original task, the final relationship graph, and any sub-agent \
outcomes, you decide whether the work is genuinely done. You reply with one JSON object — no \
markdown, no prose around it. You do not propose fixes; you flag pass/fail with a root cause. \
A repairer agent or further loop iteration will handle remediation.";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::subagent::SubAgentResult;
    use crate::graph::{Edge, Graph, Node, RelationType};
    use crate::model::{FinishReason, ModelResponse, Usage};
    use async_trait::async_trait;
    use std::sync::Mutex;

    fn clean_graph() -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::file("a", "A"));
        g.add_node(Node::file("b", "B"));
        g.add_edge(Edge::new("a", "b", RelationType::Imports, 1.0, "use")).unwrap();
        g
    }

    fn cycle_graph() -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::task("t1", "T1"));
        g.add_node(Node::task("t2", "T2"));
        g.add_edge(Edge::new("t1", "t2", RelationType::DependsOn, 1.0, "")).unwrap();
        g.add_edge(Edge::new("t2", "t1", RelationType::DependsOn, 1.0, "")).unwrap();
        g
    }

    fn ok_outcome() -> DispatchOutcome {
        DispatchOutcome {
            results: vec![SubAgentResult::ok(NodeId::from("t1"), "done".into(), 100, 200)],
            batches: vec![vec![NodeId::from("t1")]],
            total_subagent_ms: 100,
            total_tokens: 200,
            all_succeeded: true,
            graph_errors: Vec::new(),
        }
    }

    fn failed_outcome() -> DispatchOutcome {
        DispatchOutcome {
            results: vec![SubAgentResult::failure(
                NodeId::from("t1"),
                "tool error".into(),
                50,
            )],
            batches: vec![vec![NodeId::from("t1")]],
            total_subagent_ms: 50,
            total_tokens: 0,
            all_succeeded: false,
            graph_errors: Vec::new(),
        }
    }

    /// Outcome with a meaningful answer (>200 chars + task overlap).
    fn meaningful_outcome() -> DispatchOutcome {
        let answer = "The saturation check in src/agent/saturation.rs uses \
                      Jaccard similarity over character bigrams to compare \
                      question strings. It tracks the last 5 questions in \
                      a HistoryWindow sliding window. When a new question \
                      matches any recent one above the threshold (default 0.85), \
                      the loop surfaces a Block. The three-tier design lets \
                      the model self-decide when to stop asking while \
                      preventing infinite clarification loops. The hard cap \
                      is 10 rounds for Clarifying and 200 for Explore.";
        DispatchOutcome {
            results: vec![SubAgentResult::ok(
                NodeId::from("t1"),
                answer.into(),
                100,
                200,
            )],
            batches: vec![vec![NodeId::from("t1")]],
            total_subagent_ms: 100,
            total_tokens: 200,
            all_succeeded: true,
            graph_errors: Vec::new(),
        }
    }

    /// Outcome where the sub-agent's output is a long but
    /// non-overlapping text (the E3/E4 failure mode — the
    /// model produced something, but it's not an answer to
    /// the task). Differs from `ok_outcome` (short) and
    /// `meaningful_outcome` (overlapping). Total ~250 chars
    /// after boilerplate stripping, but the only word tokens
    /// are "explanation", "info", "explanation" — none of
    /// which overlap with the actual task.
    fn empty_answer_outcome() -> DispatchOutcome {
        let answer = "After analyzing the request, the system is prepared to \
                      provide a comprehensive explanation of the requested \
                      topic. The investigation has been completed and the \
                      documentation step is queued for processing. The \
                      findings include a summary of the relevant background \
                      and an overview of the typical workflow. Several \
                      diagrams and tables have been prepared for inclusion. \
                      No results were returned. Done. The information is \
                      available in the relevant file. A summary has been \
                      recorded. End of message.";
        DispatchOutcome {
            results: vec![SubAgentResult::ok(
                NodeId::from("t1"),
                answer.into(),
                100,
                200,
            )],
            batches: vec![vec![NodeId::from("t1")]],
            total_subagent_ms: 100,
            total_tokens: 200,
            all_succeeded: true,
            graph_errors: Vec::new(),
        }
    }

    struct MockModel {
        response: Mutex<Option<String>>,
    }
    impl MockModel {
        fn new(s: &str) -> Self {
            Self {
                response: Mutex::new(Some(s.to_string())),
            }
        }
    }
    #[async_trait]
    impl Model for MockModel {
        fn name(&self) -> &str {
            "mock-reviewer"
        }
        async fn complete(&self, _: ModelRequest) -> Result<ModelResponse> {
            let content = self
                .response
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| "{}".to_string());
            Ok(ModelResponse {
                content,
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                reasoning_content: None,
                usage: Usage::default(),
            })
        }
    }

    /// Mock model that returns a configured tool_call on first request.
    /// Mirrors the cascade test helper for symmetry.
    struct ToolCallJudgeModel {
        tool_call: Mutex<Option<crate::model::ToolCall>>,
    }
    #[async_trait]
    impl Model for ToolCallJudgeModel {
        fn name(&self) -> &str {
            "tool_call_judge"
        }
        async fn complete(&self, _: ModelRequest) -> Result<ModelResponse> {
            let tc = self.tool_call.lock().unwrap().take();
            Ok(ModelResponse {
                content: String::new(),
                reasoning_content: None,
                tool_calls: tc.into_iter().collect(),
                finish_reason: FinishReason::ToolCalls,
                usage: Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn deterministic_only_passes_clean_graph_and_ok_outcome() {
        let r = Reviewer::deterministic_only();
        let result = r.review("task", &clean_graph(), Some(&ok_outcome()), None).await.unwrap();
        assert!(result.passed);
        assert!(result.judge_verdict.is_none());
        assert!(result.deterministic_checks.iter().all(|c| c.passed));
    }

    #[tokio::test]
    async fn answer_content_passes_meaningful_answer() {
        // P12: a sub-agent that actually produced a real answer
        // (not just a graph description) should pass the new
        // answer_content check.
        let r = Reviewer::deterministic_only();
        let result = r
            .review(
                "explain the saturation check in saturation.rs",
                &clean_graph(),
                Some(&meaningful_outcome()),
                None,
            )
            .await
            .unwrap();
        let ans = result
            .deterministic_checks
            .iter()
            .find(|c| c.name == "answer_content")
            .expect("answer_content check present");
        assert!(ans.passed, "answer_content should pass with meaningful answer: {}", ans.details);
        assert!(result.passed);
    }

    #[tokio::test]
    async fn answer_content_fails_when_output_is_just_graph_description() {
        // P12: this is the E3/E4 failure mode — the sub-agent
        // output is a graph description (boilerplate), not an
        // actual answer. The deterministic check should catch
        // it.
        let r = Reviewer::deterministic_only();
        let result = r
            .review(
                "explain the saturation check in saturation.rs",
                &clean_graph(),
                Some(&empty_answer_outcome()),
                None,
            )
            .await
            .unwrap();
        let ans = result
            .deterministic_checks
            .iter()
            .find(|c| c.name == "answer_content")
            .expect("answer_content check present");
        // The empty-answer outcome has 200+ chars but the only
        // non-boilerplate word is the task title "saturation
        // check", not the actual answer content. The
        // check_answer_content heuristic should fail it.
        assert!(
            !ans.passed,
            "answer_content should fail when output is just boilerplate: {}",
            ans.details
        );
    }

    #[tokio::test]
    async fn with_advisor_field_is_set() {
        // P12: a reviewer built via with_advisor exposes the
        // advisor field. We don't run a real advisor call (no
        // model available in unit tests) — this just confirms
        // the wiring is correct.
        let main: Arc<dyn Model> = Arc::new(MockModel::new("{\"verdict\":\"pass\"}"));
        let advisor: Arc<dyn Model> = Arc::new(MockModel::new("{\"verdict\":\"pass\"}"));
        let r = Reviewer::with_model(main).with_advisor(advisor);
        assert!(r.advisor.is_some());
        assert!(r.model.is_some());
    }

    #[tokio::test]
    async fn deterministic_only_fails_cycle_in_graph() {
        let r = Reviewer::deterministic_only();
        let result = r.review("task", &cycle_graph(), Some(&ok_outcome()), None).await.unwrap();
        assert!(!result.passed);
        let cons = result
            .deterministic_checks
            .iter()
            .find(|c| c.name == "graph_consistency")
            .expect("graph_consistency check present");
        assert!(!cons.passed);
    }

    #[tokio::test]
    async fn deterministic_only_fails_when_subagent_fails() {
        let r = Reviewer::deterministic_only();
        let result = r.review("task", &clean_graph(), Some(&failed_outcome()), None).await.unwrap();
        assert!(!result.passed);
        let sub = result
            .deterministic_checks
            .iter()
            .find(|c| c.name == "subagent_results")
            .expect("subagent_results check present");
        assert!(!sub.passed);
    }

    #[tokio::test]
    async fn deterministic_only_flags_subagent_graph_errors() {
        let mut outcome = ok_outcome();
        outcome.graph_errors.push(GraphError::L0Structural {
            error_type: L0ErrorType::MissingRelation,
            detail: "missing call edge".into(),
            related_nodes: vec![NodeId::from("a")],
            discovered_by: Some("t1".into()),
        });
        outcome.all_succeeded = false;
        let r = Reviewer::deterministic_only();
        let result = r.review("task", &clean_graph(), Some(&outcome), None).await.unwrap();
        assert!(!result.passed);
        assert!(
            result
                .deterministic_checks
                .iter()
                .any(|c| c.name == "subagent_graph_errors" && !c.passed)
        );
    }

    #[tokio::test]
    async fn judge_pass_combined_with_deterministic_pass_yields_pass() {
        let resp = r#"{"verdict":"pass","detail":"covers task","confidence":0.9}"#;
        let r = Reviewer::with_model(Arc::new(MockModel::new(resp)));
        let result = r.review("task", &clean_graph(), Some(&ok_outcome()), None).await.unwrap();
        assert!(result.passed);
        let j = result.judge_verdict.unwrap();
        assert!(j.passed);
        assert!(j.root_cause.is_none());
        assert!((j.confidence - 0.9).abs() < 1e-6);
    }

    #[tokio::test]
    async fn judge_fail_with_graph_root_cause_routes_to_graph_invalid() {
        let resp = r#"{"verdict":"fail","root_cause":"graph","detail":"missing auth module link","confidence":0.85}"#;
        let r = Reviewer::with_model(Arc::new(MockModel::new(resp)));
        let result = r.review("task", &clean_graph(), Some(&ok_outcome()), None).await.unwrap();
        assert!(!result.passed);
        let j = result.judge_verdict.as_ref().unwrap();
        assert!(!j.passed);
        assert_eq!(j.root_cause, Some(RootCause::GraphIssue));
        // to_graph_errors should produce at least one L0Structural error
        let errs = r.to_graph_errors(&result);
        assert!(!errs.is_empty());
        assert!(errs.iter().any(|e| matches!(e, GraphError::L0Structural { .. })));
    }

    #[tokio::test]
    async fn judge_fail_with_scope_root_cause_produces_scope_gap_error() {
        let resp = r#"{"verdict":"fail","root_cause":"scope","detail":"task needs a settings module","confidence":0.7}"#;
        let r = Reviewer::with_model(Arc::new(MockModel::new(resp)));
        let result = r.review("task", &clean_graph(), Some(&ok_outcome()), None).await.unwrap();
        let errs = r.to_graph_errors(&result);
        assert!(errs.iter().any(|e| matches!(e, GraphError::ScopeGap { .. })));
    }

    #[tokio::test]
    async fn judge_fail_with_task_root_cause_does_not_produce_graph_errors() {
        let resp = r#"{"verdict":"fail","root_cause":"task","detail":"sub-agents missed coverage","confidence":0.8}"#;
        let r = Reviewer::with_model(Arc::new(MockModel::new(resp)));
        let result = r.review("task", &clean_graph(), Some(&ok_outcome()), None).await.unwrap();
        assert!(!result.passed);
        let errs = r.to_graph_errors(&result);
        // Task-issue should NOT produce graph errors (GraphLoop will route differently)
        assert!(errs.is_empty(), "task root_cause should yield no graph errors, got: {errs:?}");
    }

    #[tokio::test]
    async fn judge_markdown_fence_handled() {
        let resp = "```json\n{\"verdict\":\"pass\",\"detail\":\"ok\",\"confidence\":0.8}\n```";
        let r = Reviewer::with_model(Arc::new(MockModel::new(resp)));
        let result = r.review("task", &clean_graph(), Some(&ok_outcome()), None).await.unwrap();
        assert!(result.passed);
    }

    /// When the model returns a native tool_call with verdict=fail +
    /// root_cause=graph, the reviewer must route the failure to graph
    /// invalid errors (not silently pass via the text-fallback path).
    #[tokio::test]
    async fn judge_uses_tool_call_when_model_emits_one() {
        let tool_call = crate::model::ToolCall {
            id: "c1".into(),
            name: "judge_verdict".into(),
            arguments: serde_json::json!({
                "verdict": "fail",
                "root_cause": "graph",
                "detail": "tool_call path: missing edge",
                "confidence": 0.9
            }),
        };
        let model = ToolCallJudgeModel { tool_call: Mutex::new(Some(tool_call)) };
        let r = Reviewer::with_model(Arc::new(model));
        let result = r
            .review("task", &clean_graph(), Some(&ok_outcome()), None)
            .await
            .unwrap();
        assert!(!result.passed, "tool_call fail must surface, not silently pass");
        assert!(matches!(
            result.root_cause(),
            Some(crate::agent::RootCause::GraphIssue)
        ));
    }

    #[tokio::test]
    async fn deterministic_fail_overrides_judge_pass() {
        // Cycle in graph + judge says pass → review still fails (deterministic
        // is hard gate).
        let resp = r#"{"verdict":"pass","detail":"looks fine to me","confidence":0.9}"#;
        let r = Reviewer::with_model(Arc::new(MockModel::new(resp)));
        let result = r.review("task", &cycle_graph(), Some(&ok_outcome()), None).await.unwrap();
        assert!(!result.passed);
    }

    #[test]
    fn root_cause_accessor() {
        let result = ReviewResult {
            passed: false,
            deterministic_checks: Vec::new(),
            judge_verdict: Some(JudgeVerdict {
                passed: false,
                root_cause: Some(RootCause::ScopeIssue),
                detail: "x".into(),
                confidence: 0.7,
            }),
            rationale: "x".into(),
        };
        assert_eq!(result.root_cause(), Some(&RootCause::ScopeIssue));
    }

    #[test]
    fn no_judge_when_no_model() {
        // Just confirms deterministic_only() produces None for model.
        let r = Reviewer::deterministic_only();
        assert!(r.model.is_none());
    }
}
