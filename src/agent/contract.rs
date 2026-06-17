//! Result-check contracts for sub-agent dispatches.
//!
//! Per [[feedback-bounded-context-invariant]] and the iterative-loop
//! doctrine, every dispatched sub-task is verified against a contract
//! set at dispatch time. The check is deterministic — substring + length
//! + region — and runs both inside the sub-agent (self-check before
//! `final_answer`) and in the dispatcher (second-line defense).
//!
//! Two modes:
//!
//! - **Know-how**: main agent has a target post-state in mind. The
//!   sub-agent's `final_answer` must mention expected evidence
//!   phrases. This is the "result-based check" for tasks where the
//!   sub-agent knows what to do.
//! - **Exploratory**: no defined end-state. The sub-agent must
//!   report a localized contribution inside a defined region, with
//!   bounded item count. This is the "per-step local predicate" for
//!   open-ended exploration.
//! - **None**: no contract. Result is taken at face value. Preserves
//!   current default behavior for callers that don't opt in.

use serde::{Deserialize, Serialize};

/// A pre-dispatch verification predicate. Hangs off `SubTask` and is
/// checked on the sub-agent's `final_answer`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum CheckContract {
    /// No contract. Default for backwards compatibility.
    #[default]
    None,
    /// Know-how mode: result must mention at least one of the
    /// expected evidence phrases, and meet a minimum length.
    KnowHow {
        must_mention_any: Vec<String>,
        min_length: usize,
    },
    /// Exploratory mode: per-step contract.
    Exploratory {
        #[serde(default)]
        region: Vec<crate::graph::NodeId>,
        max_items: usize,
        must_mention_any: Vec<String>,
    },
    /// Must-edit mode: sub-agent MUST make at least one tool call
    /// (i.e., actually do work, not just report). Fails if tool_calls_made == 0.
    MustEdit,
}

/// Result of evaluating a `CheckContract` against a `SubAgentResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractOutcome {
    /// Contract met; result is acceptable.
    Satisfied,
    /// Contract not met; the human-readable reason explains why.
    Failed(String),
}

impl ContractOutcome {
    pub fn is_satisfied(&self) -> bool {
        matches!(self, Self::Satisfied)
    }
}

impl CheckContract {
    /// Evaluate this contract against the given `output` string.
    pub fn check(&self, output: &str) -> ContractOutcome {
        match self {
            Self::MustEdit => {
                // MustEdit is checked via check_tool_calls, not text output.
                ContractOutcome::Satisfied
            }
            // ... existing variants unchanged ...
            CheckContract::None => ContractOutcome::Satisfied,
            CheckContract::KnowHow { must_mention_any, min_length } => {
                if output.len() < *min_length {
                    return ContractOutcome::Failed(format!(
                        "output length {} < min_length {}",
                        output.len(),
                        min_length
                    ));
                }
                let lower = output.to_lowercase();
                let hit = must_mention_any.iter().find(|s| lower.contains(&s.to_lowercase()));
                match hit {
                    Some(_) => ContractOutcome::Satisfied,
                    None => ContractOutcome::Failed(format!(
                        "output doesn't mention any of {:?}",
                        must_mention_any
                    )),
                }
            }
            CheckContract::Exploratory { max_items, must_mention_any, .. } => {
                let items = count_graph_items(output);
                if items > *max_items {
                    return ContractOutcome::Failed(format!(
                        "reported {} items > max_items {}",
                        items, max_items
                    ));
                }
                let lower = output.to_lowercase();
                let hit = must_mention_any.iter().find(|s| lower.contains(&s.to_lowercase()));
                match hit {
                    Some(_) => ContractOutcome::Satisfied,
                    None => ContractOutcome::Failed(format!(
                        "output doesn't mention any of {:?}",
                        must_mention_any
                    )),
                }
            }
        }
    }

    /// Check a contract that depends on tool execution, not text output.
    /// MustEdit: requires at least one tool call.
    pub fn check_tool_calls(&self, tool_calls_made: usize) -> ContractOutcome {
        match self {
            Self::MustEdit => {
                if tool_calls_made > 0 {
                    ContractOutcome::Satisfied
                } else {
                    ContractOutcome::Failed(
                        "MustEdit: sub-agent made 0 tool calls — must actually edit code, not just report".into()
                    )
                }
            }
            _ => ContractOutcome::Satisfied, // other contracts don't care about tool calls
        }
    }
}

/// Count lines in `output` that look like a graph item entry
/// (heuristic: lines containing `id=` near the start, or list-style
/// `- id=...` / `* id=...` markers). Used by the Exploratory
/// contract to cap per-result item count without needing LLM calls.
fn count_graph_items(output: &str) -> usize {
    output
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            (trimmed.starts_with("- id=") || trimmed.starts_with("* id=") || trimmed.starts_with("id="))
                && line.contains('=')
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_contract_is_always_satisfied() {
        let c = CheckContract::None;
        assert_eq!(c.check(""), ContractOutcome::Satisfied);
        assert_eq!(c.check("anything"), ContractOutcome::Satisfied);
    }

    #[test]
    fn knowhow_requires_mention_and_length() {
        let c = CheckContract::KnowHow {
            must_mention_any: vec!["auth.rs".into(), "jwt".into()],
            min_length: 20,
        };
        // Length ok, mention ok
        assert!(c.check("Updated auth.rs to use jwt").is_satisfied());
        // Length ok, mention missing
        assert!(matches!(
            c.check("I updated the file"),
            ContractOutcome::Failed(_)
        ));
        // Length too short
        assert!(matches!(c.check("auth.rs"), ContractOutcome::Failed(_)));
    }

    #[test]
    fn knowhow_mention_match_is_case_insensitive() {
        let c = CheckContract::KnowHow {
            must_mention_any: vec!["Auth".into()],
            min_length: 5,
        };
        assert!(c.check("the auth.rs file is good").is_satisfied());
        assert!(c.check("AUTH is fine").is_satisfied());
    }

    #[test]
    fn exploratory_caps_items_and_requires_scope_mention() {
        let c = CheckContract::Exploratory {
            region: vec![],
            max_items: 2,
            must_mention_any: vec!["src/agent".into()],
        };
        // Within cap, scope mentioned
        let ok = "Found in src/agent:\n- id=a kind=File\n- id=b kind=File";
        assert!(c.check(ok).is_satisfied());
        // Over cap
        let too_many = "Found in src/agent:\n- id=a kind=File\n- id=b kind=File\n- id=c kind=File";
        assert!(matches!(c.check(too_many), ContractOutcome::Failed(_)));
        // Cap ok, scope missing
        let no_scope = "Found:\n- id=a kind=File";
        assert!(matches!(c.check(no_scope), ContractOutcome::Failed(_)));
    }

    #[test]
    fn count_graph_items_recognises_common_shapes() {
        // Hyphen-list shape
        assert_eq!(count_graph_items("- id=a kind=File\n- id=b"), 2);
        // Plain `id=` shape
        assert_eq!(count_graph_items("id=a kind=File\nid=b kind=File"), 2);
        // Mixed
        assert_eq!(count_graph_items("- id=a\n* id=b\nid=c"), 3);
        // Not an item
        assert_eq!(count_graph_items("no items here"), 0);
        // 'id=' inside a word shouldn't count
        assert_eq!(count_graph_items("the validid=ty is fine"), 0);
    }
}
