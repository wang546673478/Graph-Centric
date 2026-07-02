//! Saturation checks for `Clarifying` (ask_user) and `Explore` rounds.
//!
//! Per the v2 agent-harness spec, the model is free to keep asking or
//! exploring, but the loop must surface a `Block` once either of these
//! signals is observed:
//!
//! - **Similarity saturation**: a new question matches one of the
//!   recent questions above the configured threshold (default 0.85).
//! - **Count saturation**: the consecutive-round counter has reached
//!   the configured soft upper bound (Clarifying 10, Explore 200).
//!
//! `Block` here is delivered as `LoopState::Paused` with a `[block]`
//! prefix on the question, matching the convention used by the
//! `ProposerStep::Block` arm. The web UI distinguishes this from a
//! regular `ask_user` via the prefix.
//!
//! ## Why Jaccard on tokens (not embeddings)
//!
//! - O(n) per check, no extra dependencies.
//! - Stable across languages (works for Chinese `ask_user` questions
//!   that may have no word boundaries in latin senses).
//! - The threshold is tuned for *intent similarity*, not exact-phrase
//!   detection; Jaccard on character bigrams is a reasonable middle
//!   ground. Cosine on sentence embeddings would be more accurate but
//!   adds model-call latency to a check that runs once per round.

use std::collections::{HashSet, VecDeque};

/// Tokenize a string into character bigrams (lowercased).
/// Empty input → empty set.
fn bigrams(s: &str) -> HashSet<String> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return HashSet::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 {
        // Single-character input: return the single char as a set so
        // any same-letter overlap is detected (no division by zero).
        let mut out = HashSet::new();
        out.insert(chars[0].to_string());
        return out;
    }
    chars
        .windows(2)
        .map(|w| format!("{}{}", w[0], w[1]))
        .collect()
}

/// Jaccard similarity over character bigrams. Returns 0.0 for empty
/// inputs (so a 0-history check trivially passes for any new text).
pub fn jaccard(a: &str, b: &str) -> f64 {
    let sa = bigrams(a);
    let sb = bigrams(b);
    if sa.is_empty() && sb.is_empty() {
        return 0.0;
    }
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Returns true if `new_q` matches *any* question in `history` above
/// `threshold`. The history is treated as a sliding window — callers
/// truncate to the configured window size before calling.
pub fn matches_history(new_q: &str, history: &VecDeque<String>, threshold: f64) -> bool {
    history.iter().any(|h| jaccard(new_q, h) >= threshold)
}

/// A bounded sliding window over recent question texts.
pub struct HistoryWindow {
    window: VecDeque<String>,
    capacity: usize,
}

impl HistoryWindow {
    pub fn new(capacity: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, q: String) {
        if self.capacity == 0 {
            return;
        }
        if self.window.len() >= self.capacity {
            self.window.pop_front();
        }
        self.window.push_back(q);
    }

    pub fn as_deque(&self) -> &VecDeque<String> {
        &self.window
    }

    pub fn clear(&mut self) {
        self.window.clear();
    }
}

/// Result of a saturation check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaturationVerdict {
    /// The new question is fine — proceed normally.
    Proceed,
    /// A repeat detected above the configured similarity threshold.
    /// Caller should surface `Block("repeating the same question")`.
    Repeat,
    /// The count cap has been reached. Caller should surface
    /// `Block("information density saturated")` (Clarifying) or
    /// `Block("exploration did not converge")` (Explore).
    CountLimit,
}

/// Saturation state shared by Clarifying and Explore.
pub struct SaturationState {
    pub count: u32,
    pub max: u32,
    pub threshold: f64,
    pub history: HistoryWindow,
    /// When `Some(n)`, the loop should also inject a soft hint at
    /// count == n. None disables the hint.
    pub soft_hint_at: Option<u32>,
    /// When `Some(n)`, the loop should inject a hard warning at
    /// count == n. None disables the warning.
    pub hard_hint_at: Option<u32>,
    /// Whether the soft hint has already been injected for the
    /// current streak (so we hint once, not every round).
    pub soft_hint_sent: bool,
    pub hard_hint_sent: bool,
}

impl SaturationState {
    pub fn new(max: u32, threshold: f64, history_capacity: usize) -> Self {
        Self {
            count: 0,
            max,
            threshold,
            history: HistoryWindow::new(history_capacity),
            soft_hint_at: None,
            hard_hint_at: None,
            soft_hint_sent: false,
            hard_hint_sent: false,
        }
    }

    pub fn with_tier_hints(mut self, soft: Option<u32>, hard: Option<u32>) -> Self {
        self.soft_hint_at = soft;
        self.hard_hint_at = hard;
        self
    }

    /// Inspect a new question. Does NOT mutate `count`; the caller
    /// decides whether to record it (typically: only on the
    /// `Proceed` path, so a `Block` doesn't itself bump the count).
    pub fn inspect(&self, new_q: &str) -> SaturationVerdict {
        if self.count >= self.max {
            return SaturationVerdict::CountLimit;
        }
        if matches_history(new_q, self.history.as_deque(), self.threshold) {
            return SaturationVerdict::Repeat;
        }
        SaturationVerdict::Proceed
    }

    /// Record a successful Proceed — increments count and pushes to
    /// history.
    pub fn record(&mut self, new_q: String) {
        self.count = self.count.saturating_add(1);
        self.history.push(new_q);
    }

    /// Reset the counter + history (used when the model emits a
    /// `propose_patch` or otherwise signals progress).
    pub fn reset(&mut self) {
        self.count = 0;
        self.history.clear();
        self.soft_hint_sent = false;
        self.hard_hint_sent = false;
    }

    /// Returns the tier hint to inject this round, if any. Caller is
    /// responsible for actually pushing the hint into the
    /// conversation and calling `mark_soft_hint_sent` /
    /// `mark_hard_hint_sent`.
    pub fn tier_hint_to_inject(&self) -> Option<TierHint> {
        if let Some(at) = self.hard_hint_at {
            if self.count >= at && !self.hard_hint_sent {
                return Some(TierHint::Hard);
            }
        }
        if let Some(at) = self.soft_hint_at {
            if self.count >= at && !self.soft_hint_sent {
                return Some(TierHint::Soft);
            }
        }
        None
    }

    pub fn mark_soft_hint_sent(&mut self) {
        self.soft_hint_sent = true;
    }

    pub fn mark_hard_hint_sent(&mut self) {
        self.hard_hint_sent = true;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierHint {
    Soft,
    Hard,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jaccard_identical_strings_is_one() {
        assert!((jaccard("hello world", "hello world") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn jaccard_disjoint_strings_is_zero() {
        assert!((jaccard("abcdef", "ghijkl") - 0.0).abs() < 1e-9);
    }

    #[test]
    fn jaccard_handles_empty_input() {
        assert_eq!(jaccard("", ""), 0.0);
        assert_eq!(jaccard("", "abc"), 0.0);
        assert_eq!(jaccard("abc", ""), 0.0);
    }

    #[test]
    fn jaccard_chinese_works_on_bigrams() {
        // Two Chinese sentences about the same topic should score
        // higher than the unrelated-topic baseline. The two share
        // 你/什/语言/等 but diverge in 编写/使用, so the score sits
        // in the 0.2-0.3 range — enough to beat the unrelated
        // baseline (~0.05) but well below the 0.85 repeat threshold,
        // which is the property we actually care about.
        let a = "你是什么语言编写的?";
        let b = "你使用什么语言实现的?";
        let s = jaccard(a, b);
        assert!(s > 0.2, "expected some overlap, got {s}");
        // Well below the configured threshold — this is the "related
        // but not a repeat" zone the system relies on.
        assert!(s < 0.85, "should not yet be a repeat, got {s}");
    }

    #[test]
    fn jaccard_low_overlap_for_unrelated_topics() {
        let a = "请说明交付物是什么";
        let b = "你使用什么数据库存储";
        let s = jaccard(a, b);
        assert!(s < 0.4, "expected low overlap, got {s}");
    }

    #[test]
    fn matches_history_detects_repeat() {
        let mut h = VecDeque::new();
        h.push_back("你使用什么语言?".to_string());
        h.push_back("目标用户是?".to_string());
        assert!(matches_history("你使用什么语言?", &h, 0.85));
        assert!(!matches_history("数据存储在哪?", &h, 0.85));
    }

    #[test]
    fn history_window_respects_capacity() {
        let mut w = HistoryWindow::new(2);
        w.push("a".to_string());
        w.push("b".to_string());
        w.push("c".to_string());
        assert_eq!(w.as_deque().len(), 2);
        assert_eq!(w.as_deque()[0], "b");
        assert_eq!(w.as_deque()[1], "c");
    }

    #[test]
    fn history_window_zero_capacity_is_noop() {
        let mut w = HistoryWindow::new(0);
        w.push("a".to_string());
        assert!(w.as_deque().is_empty());
    }

    #[test]
    fn saturation_proceeds_on_first_question() {
        let s = SaturationState::new(10, 0.85, 5);
        assert_eq!(s.inspect("first question"), SaturationVerdict::Proceed);
    }

    #[test]
    fn saturation_blocks_on_count_cap() {
        let mut s = SaturationState::new(3, 0.85, 5);
        for i in 0..3 {
            s.record(format!("q{i}"));
        }
        assert_eq!(s.inspect("any"), SaturationVerdict::CountLimit);
    }

    #[test]
    fn saturation_blocks_on_similarity_repeat() {
        let mut s = SaturationState::new(10, 0.85, 5);
        s.record("你使用什么语言?".to_string());
        // Same question, should trigger Repeat
        assert_eq!(
            s.inspect("你使用什么语言?"),
            SaturationVerdict::Repeat
        );
    }

    #[test]
    fn saturation_reset_clears_everything() {
        let mut s = SaturationState::new(10, 0.85, 5);
        s.record("q1".to_string());
        s.record("q2".to_string());
        s.reset();
        assert_eq!(s.count, 0);
        assert!(s.history.as_deque().is_empty());
    }

    #[test]
    fn tier_hints_emit_at_thresholds() {
        let mut s = SaturationState::new(200, 0.85, 5)
            .with_tier_hints(Some(100), Some(150));
        // Count up to 99: no hint
        for i in 0..99 {
            s.record(format!("q{i}"));
        }
        assert_eq!(s.tier_hint_to_inject(), None);
        // 100: soft hint
        s.record("q99".to_string());
        assert_eq!(s.tier_hint_to_inject(), Some(TierHint::Soft));
        s.mark_soft_hint_sent();
        assert_eq!(s.tier_hint_to_inject(), None);
        // 150: hard hint
        for i in 100..150 {
            s.record(format!("q{i}"));
        }
        assert_eq!(s.tier_hint_to_inject(), Some(TierHint::Hard));
        s.mark_hard_hint_sent();
        assert_eq!(s.tier_hint_to_inject(), None);
    }
}
