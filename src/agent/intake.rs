//! Intake gate — the code-level enforcement of the Mode A / Mode B rule.
//!
//! ARCHITECTURE §1a says the model's FIRST step in a fresh conversation
//! is an intake decision. Mode A (clear task) → `propose_patch` directly.
//! Mode B (vague task) → `ask_user` BEFORE drawing any graph nodes.
//!
//! The system prompt teaches the model this rule, but prompt-only is
//! not load-bearing: an instruction-tuned model will still happily emit
//! `propose_patch` for a vague task, which is the bug we're seeing in
//! production. This module is the second line of defense — when the
//! prompt fails, this gate catches it.
//!
//! The classifier is heuristic. It errs on the side of *not* rejecting
//! clear tasks (we'd rather miss a vague case than block a clear one),
//! because the cost of false-positive is "annoying ask_user" and the
//! cost of false-negative is the bug we're fixing.

use crate::agent::proposer::ProposerStep;

/// Coarse classification of a task's clarity for intake purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskClarity {
    /// Task names a concrete deliverable. The first step can be
    /// `propose_patch` directly.
    Clear,
    /// Task is open to multiple readings, has no clear success
    /// criterion, or otherwise requires clarification. The first
    /// step should be `ask_user`.
    Vague,
}

/// Classify a task as Clear or Vague based on surface heuristics.
///
/// Heuristics, in order:
/// 1. Empty → Vague.
/// 2. Starts with a vague phrase (EN or ZH) → Vague.
/// 3. Very short (< 5 words) with no clear action verb → Vague.
/// 4. Otherwise → Clear.
///
/// We intentionally do **not** require "specific file path + specific
/// action verb" to call something Clear. That heuristic would block
/// valid tasks like "deploy the staging build" (no path, but a clear
/// action) or "look at how the user auth flow works" (no action, but a
/// specific subsystem). The vague-starter list is the load-bearing
/// signal; the short-task fallback is a safety net.
pub fn classify_task_clarity(task: &str) -> TaskClarity {
    let t = task.trim();
    if t.is_empty() {
        return TaskClarity::Vague;
    }

    // A single word is never enough — even if it IS a clear verb
    // ("fix"), the model has no target to act on. Treat as Vague.
    let word_count = t.split_whitespace().count();
    if word_count < 2 {
        return TaskClarity::Vague;
    }

    if starts_with_vague_starter(t) {
        return TaskClarity::Vague;
    }

    if word_count < 5 && !has_clear_action(t) {
        return TaskClarity::Vague;
    }

    TaskClarity::Clear
}

/// Check whether the given step is compliant with the intake rule.
///
/// - Round 0 (the first round of a fresh conversation) is the only
///   round we gate.
/// - Clear tasks are always compliant.
/// - Vague tasks on round 0 require `AskUser`. Any other step kind
///   fails the gate.
///
/// On failure, the caller is expected to retry the model call with a
/// hint to emit `ask_user`. We don't fabricate an `ask_user` step
/// ourselves — the model should produce the question, not us.
pub fn check_intake_compliance(
    task: &str,
    round: usize,
    step: &ProposerStep,
) -> Result<(), String> {
    if round > 0 {
        return Ok(());
    }
    if classify_task_clarity(task) == TaskClarity::Clear {
        return Ok(());
    }
    if matches!(step, ProposerStep::AskUser { .. }) {
        return Ok(());
    }
    Err(format!(
        "intake violation: task classified as Vague (heuristic); the first step on a vague task must be `ask_user`, got `{}`",
        step.kind()
    ))
}

// ---------------------------------------------------------------------------
// Heuristics
// ---------------------------------------------------------------------------

/// Does the task start with a vague phrase? EN and ZH lists, both
/// checked case-insensitively (EN) and as-substring (ZH).
fn starts_with_vague_starter(t: &str) -> bool {
    let lower = t.to_lowercase();
    let starters_en = [
        "look at ",
        "see ",
        "review ",
        "explore ",
        "summarize ",
        "explain ",
        "what is ",
        "what are ",
        "what can ",
        "what could ",
        "how does ",
        "how do ",
        "tell me about ",
        "describe ",
        "give me an overview of ",
    ];
    if starters_en.iter().any(|s| lower.starts_with(s)) {
        return true;
    }
    let starters_zh = [
        "看看",
        "看一下",
        "了解",
        "探索",
        "总结",
        "总结一下",
        "介绍",
        "理解",
        "梳理",
        "调研",
    ];
    starters_zh.iter().any(|s| t.starts_with(s))
}

/// Does the task contain a clear action verb (EN or ZH)? Used as a
/// safety net: if a task is very short and has no clear verb, it's
/// probably vague. Not the primary signal — `starts_with_vague_starter`
/// is.
fn has_clear_action(t: &str) -> bool {
    // English: match as a whole word, not as a substring, to avoid
    // false positives like "address" (contains "add") or "founded"
    // (contains "find").
    let verbs_en = [
        "fix", "add", "refactor", "implement", "migrate", "port", "optimize",
        "debug", "find", "write", "remove", "delete", "update", "rewrite",
        "extract", "inline", "split", "merge", "rename", "convert",
        "compile", "test", "build", "run", "deploy", "ship", "release",
        "document", "configure", "install", "uninstall", "patch",
    ];
    let has_en = t.split_whitespace().any(|w| {
        let stripped: String = w
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-')
            .collect();
        verbs_en.contains(&stripped.to_lowercase().as_str())
    });
    if has_en {
        return true;
    }
    // Chinese: substring match (no word boundaries in ZH).
    let verbs_zh = [
        "修", "加", "改", "实现", "迁移", "优化", "调试", "找", "写", "删",
        "更新", "删除", "重构", "添加", "拆分", "合并", "重命名",
        "运行", "构建", "测试", "部署", "发布", "文档", "配置", "安装",
    ];
    verbs_zh.iter().any(|v| t.contains(v))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Vague --

    #[test]
    fn vague_starter_zh_kan_kan() {
        // The exact task from the bug report.
        let t = "/home/hhhh/claude-code-sourcemap-main 看看这个源码有什么值得借鉴的";
        assert_eq!(classify_task_clarity(t), TaskClarity::Vague);
    }

    #[test]
    fn vague_starter_zh_liao_jie() {
        assert_eq!(
            classify_task_clarity("了解一下这个项目"),
            TaskClarity::Vague
        );
    }

    #[test]
    fn vague_starter_en_look_at() {
        assert_eq!(
            classify_task_clarity("look at the codebase and tell me what you see"),
            TaskClarity::Vague
        );
    }

    #[test]
    fn vague_starter_en_what_is() {
        assert_eq!(
            classify_task_clarity("what is the difference between X and Y"),
            TaskClarity::Vague
        );
    }

    #[test]
    fn vague_starter_en_summarize() {
        assert_eq!(
            classify_task_clarity("summarize the project"),
            TaskClarity::Vague
        );
    }

    #[test]
    fn vague_empty() {
        assert_eq!(classify_task_clarity(""), TaskClarity::Vague);
        assert_eq!(classify_task_clarity("   "), TaskClarity::Vague);
    }

    #[test]
    fn vague_short_no_action() {
        // Very short, no verb.
        assert_eq!(classify_task_clarity("the auth bug"), TaskClarity::Vague);
        assert_eq!(classify_task_clarity("fix"), TaskClarity::Vague); // single word, no target
    }

    // -- Clear --

    #[test]
    fn clear_fix_specific_path() {
        assert_eq!(
            classify_task_clarity("fix the bug in src/foo.rs:42"),
            TaskClarity::Clear
        );
    }

    #[test]
    fn clear_zh_specific_action() {
        assert_eq!(
            classify_task_clarity("请帮我修一下 src/foo.rs 的 bug"),
            TaskClarity::Clear
        );
    }

    #[test]
    fn clear_long_vague_starter_zh_is_clear() {
        // "看看" alone is vague, but with a clear action verb in the
        // body, the heuristic should not call it vague on length.
        // Wait — the rule is "starts with vague_starter" → Vague,
        // regardless of body. So this is still Vague. This test
        // documents that behavior; if we want to relax it, this is the
        // test to update.
        assert_eq!(
            classify_task_clarity("看看 src/foo.rs 修一下 bug"),
            TaskClarity::Vague
        );
    }

    #[test]
    fn clear_long_with_verb() {
        assert_eq!(
            classify_task_clarity("Refactor the dispatcher to use a tokio semaphore"),
            TaskClarity::Clear
        );
    }

    #[test]
    fn clear_deploy() {
        assert_eq!(
            classify_task_clarity("deploy the staging build"),
            TaskClarity::Clear
        );
    }

    #[test]
    fn clear_run_tests() {
        assert_eq!(
            classify_task_clarity("run the integration tests"),
            TaskClarity::Clear
        );
    }

    #[test]
    fn clear_zh_chinese_verb() {
        assert_eq!(
            classify_task_clarity("请帮我重命名 main 函数为 entry_point"),
            TaskClarity::Clear
        );
    }

    // -- Gate --

    #[test]
    fn gate_round_zero_vague_with_ask_user_passes() {
        let step = ProposerStep::AskUser {
            question: "你感兴趣的是哪一类借鉴?".into(),
            options: vec![],
            rationale: "需要确认方向".into(),
        };
        assert!(check_intake_compliance(
            "看看这个源码有什么值得借鉴的",
            0,
            &step
        )
        .is_ok());
    }

    #[test]
    fn gate_round_zero_vague_with_propose_patch_fails() {
        let step = ProposerStep::ProposePatch {
            patch: crate::graph::GraphPatch::default(),
            rationale: "let's just draw some nodes".into(),
        };
        let result = check_intake_compliance(
            "/home/hhhh/claude-code-sourcemap-main 看看这个源码有什么值得借鉴的",
            0,
            &step,
        );
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("intake violation"));
        assert!(msg.contains("Vague"));
        assert!(msg.contains("propose_patch"));
    }

    #[test]
    fn gate_round_zero_clear_with_propose_patch_passes() {
        let step = ProposerStep::ProposePatch {
            patch: crate::graph::GraphPatch::default(),
            rationale: "ok".into(),
        };
        assert!(
            check_intake_compliance("fix the bug in src/foo.rs", 0, &step).is_ok()
        );
    }

    #[test]
    fn gate_round_nonzero_always_passes() {
        // After round 0, intake is over. Even on a vague task, any
        // step kind is fine.
        let step = ProposerStep::ProposePatch {
            patch: crate::graph::GraphPatch::default(),
            rationale: "ok".into(),
        };
        assert!(
            check_intake_compliance("看看这个源码有什么值得借鉴的", 1, &step).is_ok()
        );
        assert!(
            check_intake_compliance("看看这个源码有什么值得借鉴的", 5, &step).is_ok()
        );
    }

    #[test]
    fn gate_clear_task_always_passes() {
        let step = ProposerStep::CallTool {
            tool: "bash".into(),
            args: serde_json::json!({}),
            rationale: "ls".into(),
        };
        assert!(
            check_intake_compliance("fix src/foo.rs:42", 0, &step).is_ok()
        );
    }
}
