//! Skill matcher — token-based scoring engine for auto-matching skills to tasks.
//!
//! No LLM calls. Pure token overlap (Jaccard) between the task text and each
//! skill's trigger + slug, with optional domain-tag boosting. Designed to run
//! in <1ms per skill so it can fire on every run creation without adding
//! perceptible latency.

use super::storage::SkillStorage;
use super::types::SkillRef;
use crate::error::HarnessError;
use std::collections::HashSet;

/// Tokenize a string into lowercase words, splitting on whitespace and
/// common punctuation: `,.;:!?()-[]{}'"`.  Consecutive non-word characters
/// are treated as delimiters; empty tokens are dropped.
fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| c.is_whitespace() || ",.;:!?()-[]{}'\"".contains(c))
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// Jaccard similarity: `|a ∩ b| / |a ∪ b|`.  Returns 0.0 when both sets are empty.
fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    intersection as f64 / union as f64
}

// Words that signal a stronger domain match — when they appear in both the
// task and a skill trigger, they carry more weight than common stop words.
const SIGNAL_WORDS: &[&str] = &[
    "refactor", "migrate", "test", "deploy", "debug", "optimize", "design",
    "implement", "fix", "build", "configure", "setup", "install", "upgrade",
    "database", "api", "auth", "login", "ui", "frontend", "backend", "cli",
    "pipeline", "ci", "cd", "docker", "kubernetes", "terraform", "monitor",
    "log", "error", "performance", "security", "audit", "review", "document",
    "generate", "parse", "validate", "transform", "migrate",
];

/// Score how well a skill matches a task [0.0, 1.0].
///
/// Algorithm:
/// 1. Tokenize task text, skill trigger, and skill slug.
/// 2. Jaccard(task, trigger) × 0.7 + Jaccard(task, slug_tokens) × 0.3
/// 3. Signal-word boost: +0.03 per signal word appearing in both task and trigger
///    (capped at +0.15).
/// 4. Clamp to [0.0, 1.0].
pub fn score_skill_match(task: &str, skill: &SkillRef) -> f64 {
    let task_tokens: Vec<String> = tokenize(task);
    let trigger_tokens: Vec<String> = tokenize(&skill.trigger);
    let slug_tokens: Vec<String> = tokenize(&skill.slug.replace('-', " "));

    let task_set: HashSet<String> = task_tokens.iter().cloned().collect();
    let trigger_set: HashSet<String> = trigger_tokens.iter().cloned().collect();
    let slug_set: HashSet<String> = slug_tokens.iter().cloned().collect();

    let trigger_overlap = jaccard(&task_set, &trigger_set);
    let slug_overlap = jaccard(&task_set, &slug_set);

    let mut score = trigger_overlap * 0.7 + slug_overlap * 0.3;

    // Signal-word boost: count how many signal words appear in both task and trigger.
    let signal_hits: usize = SIGNAL_WORDS
        .iter()
        .filter(|w| task_set.contains(&w.to_string()) && trigger_set.contains(&w.to_string()))
        .count();
    score += (signal_hits as f64 * 0.03).min(0.15);

    score.clamp(0.0, 1.0)
}

/// Find skills whose match score exceeds `threshold`, sorted descending.
pub fn find_matching_skills(
    task: &str,
    storage: &dyn SkillStorage,
    threshold: f64,
) -> std::result::Result<Vec<(SkillRef, f64)>, HarnessError> {
    let all = storage
        .list()
        .map_err(|e| HarnessError::model(format!("skill list failed: {e}")))?;
    let mut scored: Vec<(SkillRef, f64)> = all
        .into_iter()
        .map(|r| {
            let s = score_skill_match(task, &r);
            (r, s)
        })
        .filter(|(_, s)| *s >= threshold)
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scored)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_ref(slug: &str, trigger: &str) -> SkillRef {
        SkillRef {
            slug: slug.into(),
            trigger: trigger.into(),
        }
    }

    #[test]
    fn tokenize_splits_on_whitespace_and_punct() {
        let tokens = tokenize("Hello, world! How are you?");
        assert_eq!(tokens, vec!["hello", "world", "how", "are", "you"]);
    }

    #[test]
    fn tokenize_handles_empty() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("  ,.;:!?  ").is_empty());
    }

    #[test]
    fn jaccard_identical() {
        let a: HashSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let b = a.clone();
        assert!((jaccard(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn jaccard_disjoint() {
        let a: HashSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let b: HashSet<String> = ["c", "d"].iter().map(|s| s.to_string()).collect();
        assert!((jaccard(&a, &b) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn jaccard_partial() {
        let a: HashSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let b: HashSet<String> = ["b", "c", "d"].iter().map(|s| s.to_string()).collect();
        // intersection = {b, c} = 2, union = {a, b, c, d} = 4 → 0.5
        assert!((jaccard(&a, &b) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn jaccard_both_empty() {
        let a: HashSet<String> = HashSet::new();
        let b: HashSet<String> = HashSet::new();
        assert!((jaccard(&a, &b) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn full_overlap_scores_high() {
        let skill = skill_ref("refactor-db", "applies when the user asks about database refactoring");
        let score = score_skill_match("refactor the database layer", &skill);
        assert!(score > 0.2, "expected score > 0.2, got {score}");
    }

    #[test]
    fn no_overlap_scores_low() {
        let skill = skill_ref("deploy-infra", "applies when deploying infrastructure with terraform");
        let score = score_skill_match("fix the login bug in auth.rs", &skill);
        assert!(score < 0.15, "expected score < 0.15, got {score}");
    }

    #[test]
    fn empty_task_scores_zero() {
        let skill = skill_ref("anything", "applies when anything happens");
        let score = score_skill_match("", &skill);
        assert!((score - 0.0).abs() < 1e-9);
    }

    #[test]
    fn empty_trigger_scores_zero() {
        let skill = skill_ref("empty-trigger", "");
        let score = score_skill_match("do something", &skill);
        assert!((score - 0.0).abs() < 1e-9);
    }

    #[test]
    fn signal_words_boost_score() {
        let with_signal = skill_ref("s1", "applies when you need to refactor and optimize code");
        let without_signal = skill_ref("s2", "applies when you need to change and improve code");
        let task = "refactor and optimize the database layer";
        let score_signal = score_skill_match(task, &with_signal);
        let score_no_signal = score_skill_match(task, &without_signal);
        assert!(
            score_signal > score_no_signal,
            "signal words should boost: {score_signal} vs {score_no_signal}"
        );
    }

    #[test]
    fn score_is_clamped_to_one() {
        let skill = skill_ref("exact", "refactor database");
        // Task equals trigger → very high Jaccard, but should not exceed 1.0.
        let score = score_skill_match("refactor database", &skill);
        assert!(score <= 1.0, "score={score} should be <= 1.0");
    }

    #[test]
    fn slug_tokens_contribute() {
        let skill = skill_ref("database-optimization", "applies when you want to make things faster");
        let task = "optimize the database";
        let score = score_skill_match(task, &skill);
        // "optimization" in slug overlaps with "optimize" in task
        assert!(score > 0.05, "slug tokens should contribute, got {score}");
    }
}
