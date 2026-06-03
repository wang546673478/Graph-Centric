//! L1 layer — per-node semantic descriptions.
//!
//! Per the three-layer model defined in design doc v2.0
//! (see [[feedback-three-layer-graph]]):
//!
//! - **L0** is the skeleton: nodes + edges (handled by [`super::Node`] / [`super::Edge`]).
//! - **L1** is the muscle: a structured semantic description per node —
//!   what it *does*, how it's *implemented*, why it's *designed that way*,
//!   what it must *not* do. This module owns L1.
//! - **L2** is the skin: the raw bytes (source files, configs, schemas).
//!   L2 lives outside the graph and is loaded on demand via `SourceLoader`.
//!
//! [`L1Description`] is the per-node payload; [`L1Store`] is the
//! `NodeId → L1Description` map that lives alongside (or inside) a
//! [`super::Graph`].
//!
//! Per-node `confidence` and `revision` let the verifier and repairer
//! reason about staleness — when L0 changes around a node, that node's L1
//! may have drifted from L2 and needs re-enrichment.

use super::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Structured per-node semantic description.
///
/// Every field is intentionally a free-form string — the harness does not
/// impose a domain vocabulary. For code the responsibility might read
/// "validate JWT tokens"; for an infra resource it might be "VPC for
/// production frontend"; for a research concept it might be "argument
/// that priors are subjective".
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct L1Description {
    /// What this node is *for*. One sentence ideally.
    pub responsibility: String,
    /// How it does what it does. Brief — implementation strategy, key
    /// dependencies, algorithmic shape.
    pub implementation: String,
    /// Why it's designed this way. The motivation behind the implementation
    /// choices that aren't obvious from the code.
    pub design_intent: String,
    /// Important constraints / invariants / things that must remain true.
    pub constraints: String,
    /// `0.0..=1.0` confidence the model has in this description. Low
    /// confidence flags that re-enrichment may be needed.
    pub confidence: f64,
    /// Monotonically-increasing revision; bumped each time the description
    /// is rewritten. Useful for staleness checks and audit logs.
    pub revision: u32,
}

impl L1Description {
    /// Empty L1: zero-confidence placeholder for nodes that exist in L0
    /// but haven't been enriched yet.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Convenience builder.
    pub fn new(
        responsibility: impl Into<String>,
        implementation: impl Into<String>,
        design_intent: impl Into<String>,
        constraints: impl Into<String>,
    ) -> Self {
        Self {
            responsibility: responsibility.into(),
            implementation: implementation.into(),
            design_intent: design_intent.into(),
            constraints: constraints.into(),
            confidence: 0.7,
            revision: 1,
        }
    }

    pub fn with_confidence(mut self, c: f64) -> Self {
        self.confidence = c.clamp(0.0, 1.0);
        self
    }

    /// True when every textual field is empty.
    pub fn is_blank(&self) -> bool {
        self.responsibility.trim().is_empty()
            && self.implementation.trim().is_empty()
            && self.design_intent.trim().is_empty()
            && self.constraints.trim().is_empty()
    }

    /// Rough character count across all fields. Used by the verifier to
    /// budget how much L1 text the model sees in a sampling check.
    pub fn char_len(&self) -> usize {
        self.responsibility.chars().count()
            + self.implementation.chars().count()
            + self.design_intent.chars().count()
            + self.constraints.chars().count()
    }

    /// Render as a compact human-readable block. Used by `ContextBuilder`
    /// when including full L1 in the sub-agent prompt (distance 0).
    pub fn render_full(&self) -> String {
        let mut s = String::new();
        if !self.responsibility.trim().is_empty() {
            s.push_str(&format!("responsibility: {}\n", self.responsibility));
        }
        if !self.implementation.trim().is_empty() {
            s.push_str(&format!("implementation: {}\n", self.implementation));
        }
        if !self.design_intent.trim().is_empty() {
            s.push_str(&format!("design_intent: {}\n", self.design_intent));
        }
        if !self.constraints.trim().is_empty() {
            s.push_str(&format!("constraints: {}\n", self.constraints));
        }
        s
    }

    /// Render only the responsibility + implementation — used at distance 1
    /// in `ContextBuilder` where context budget is tighter.
    pub fn render_brief(&self) -> String {
        let mut s = String::new();
        if !self.responsibility.trim().is_empty() {
            s.push_str(&format!("does: {}", self.responsibility));
        }
        if !self.implementation.trim().is_empty() {
            if !s.is_empty() {
                s.push_str(" | ");
            }
            s.push_str(&format!("how: {}", self.implementation));
        }
        s
    }

    /// One-line summary: just the responsibility. Used at distance 2+.
    pub fn render_oneline(&self) -> String {
        self.responsibility.clone()
    }
}

// ---------------------------------------------------------------------------
// L1Store
// ---------------------------------------------------------------------------

/// `NodeId → L1Description` storage living alongside a [`super::Graph`].
///
/// Stored separately from `Node` to keep L0 (structural) operations cheap
/// and independent — you can ship L0 around without dragging the full L1
/// payload along, and `L1Store` can be serialised separately for
/// inspection/diffs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct L1Store {
    entries: HashMap<NodeId, L1Description>,
}

impl L1Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, id: &NodeId) -> Option<&L1Description> {
        self.entries.get(id)
    }

    pub fn get_mut(&mut self, id: &NodeId) -> Option<&mut L1Description> {
        self.entries.get_mut(id)
    }

    pub fn contains(&self, id: &NodeId) -> bool {
        self.entries.contains_key(id)
    }

    /// Insert or overwrite. Bumps `revision` when replacing an existing entry.
    pub fn set(&mut self, id: NodeId, mut desc: L1Description) {
        if let Some(prev) = self.entries.get(&id) {
            desc.revision = prev.revision.saturating_add(1);
        } else if desc.revision == 0 {
            desc.revision = 1;
        }
        self.entries.insert(id, desc);
    }

    pub fn remove(&mut self, id: &NodeId) -> Option<L1Description> {
        self.entries.remove(id)
    }

    /// Iterate `(NodeId, &L1Description)`.
    pub fn iter(&self) -> impl Iterator<Item = (&NodeId, &L1Description)> {
        self.entries.iter()
    }

    /// All node ids that have an L1 entry.
    pub fn ids(&self) -> impl Iterator<Item = &NodeId> {
        self.entries.keys()
    }

    /// Node ids that **don't** appear in this store — useful for
    /// "which nodes still need enrichment?".
    pub fn missing_among<'a, I>(&'a self, candidates: I) -> Vec<NodeId>
    where
        I: IntoIterator<Item = &'a NodeId>,
    {
        candidates
            .into_iter()
            .filter(|id| !self.contains(id))
            .cloned()
            .collect()
    }

    /// Ids whose description's confidence is below `threshold`.
    /// Used by the verifier/repairer to pick re-enrichment candidates.
    pub fn low_confidence(&self, threshold: f64) -> Vec<NodeId> {
        self.entries
            .iter()
            .filter(|(_, desc)| desc.confidence < threshold)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Ids whose description is fully blank — these are placeholders
    /// inserted by something that knew the node needs L1 but had no
    /// content yet.
    pub fn blank_ids(&self) -> Vec<NodeId> {
        self.entries
            .iter()
            .filter(|(_, desc)| desc.is_blank())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Drop entries whose key is not in `live_ids`. Returns the number of
    /// entries removed. Use this after L0 patches that remove nodes, to
    /// keep L1 from accumulating orphans.
    pub fn prune_orphans<'a, I>(&mut self, live_ids: I) -> usize
    where
        I: IntoIterator<Item = &'a NodeId>,
    {
        let alive: std::collections::HashSet<NodeId> = live_ids.into_iter().cloned().collect();
        let before = self.entries.len();
        self.entries.retain(|id, _| alive.contains(id));
        before - self.entries.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_description_is_blank() {
        assert!(L1Description::empty().is_blank());
    }

    #[test]
    fn new_clamps_confidence_via_builder() {
        let d = L1Description::new("a", "b", "c", "d").with_confidence(1.7);
        assert!((d.confidence - 1.0).abs() < f64::EPSILON);
        let d = L1Description::new("a", "b", "c", "d").with_confidence(-0.3);
        assert!(d.confidence.abs() < f64::EPSILON);
    }

    #[test]
    fn render_full_includes_only_nonblank_fields() {
        let d = L1Description {
            responsibility: "do thing".into(),
            implementation: "".into(),
            design_intent: "for reasons".into(),
            constraints: "".into(),
            confidence: 0.8,
            revision: 1,
        };
        let r = d.render_full();
        assert!(r.contains("responsibility: do thing"));
        assert!(r.contains("design_intent: for reasons"));
        assert!(!r.contains("implementation:"));
        assert!(!r.contains("constraints:"));
    }

    #[test]
    fn render_brief_combines_responsibility_and_implementation() {
        let d = L1Description::new("validate JWT", "wraps jsonwebtoken", "centralize logic", "key from env");
        let b = d.render_brief();
        assert!(b.contains("does: validate JWT"));
        assert!(b.contains("how: wraps jsonwebtoken"));
        assert!(b.contains("|"));
    }

    #[test]
    fn render_oneline_returns_responsibility() {
        let d = L1Description::new("validate JWT", "x", "y", "z");
        assert_eq!(d.render_oneline(), "validate JWT");
    }

    #[test]
    fn char_len_sums_field_chars() {
        let d = L1Description::new("一二三", "四五", "六", "七八九十");
        // 3 + 2 + 1 + 4 = 10
        assert_eq!(d.char_len(), 10);
    }

    fn id(s: &str) -> NodeId {
        NodeId::from(s)
    }

    #[test]
    fn store_set_get_remove_round_trips() {
        let mut s = L1Store::new();
        assert!(s.is_empty());
        s.set(id("a"), L1Description::new("r", "i", "d", "c"));
        assert_eq!(s.len(), 1);
        assert!(s.contains(&id("a")));
        let got = s.get(&id("a")).unwrap();
        assert_eq!(got.responsibility, "r");
        assert_eq!(got.revision, 1);
        let removed = s.remove(&id("a")).unwrap();
        assert_eq!(removed.responsibility, "r");
        assert!(s.is_empty());
    }

    #[test]
    fn set_bumps_revision_when_replacing_existing() {
        let mut s = L1Store::new();
        s.set(id("a"), L1Description::new("v1", "", "", ""));
        s.set(id("a"), L1Description::new("v2", "", "", ""));
        let got = s.get(&id("a")).unwrap();
        assert_eq!(got.responsibility, "v2");
        assert_eq!(got.revision, 2);
        s.set(id("a"), L1Description::new("v3", "", "", ""));
        assert_eq!(s.get(&id("a")).unwrap().revision, 3);
    }

    #[test]
    fn set_first_insert_starts_at_revision_one() {
        let mut s = L1Store::new();
        let mut d = L1Description::default();
        d.revision = 0;
        s.set(id("x"), d);
        assert_eq!(s.get(&id("x")).unwrap().revision, 1);
    }

    #[test]
    fn missing_among_lists_ids_without_l1() {
        let mut s = L1Store::new();
        s.set(id("a"), L1Description::new("ra", "", "", ""));
        let candidates = [id("a"), id("b"), id("c")];
        let missing = s.missing_among(candidates.iter());
        assert!(missing.contains(&id("b")));
        assert!(missing.contains(&id("c")));
        assert!(!missing.contains(&id("a")));
    }

    #[test]
    fn low_confidence_filters_by_threshold() {
        let mut s = L1Store::new();
        s.set(
            id("hi"),
            L1Description::new("h", "h", "h", "h").with_confidence(0.9),
        );
        s.set(
            id("lo"),
            L1Description::new("l", "l", "l", "l").with_confidence(0.4),
        );
        let low = s.low_confidence(0.6);
        assert_eq!(low, vec![id("lo")]);
    }

    #[test]
    fn blank_ids_returns_placeholder_entries() {
        let mut s = L1Store::new();
        s.set(id("blank"), L1Description::empty());
        s.set(id("full"), L1Description::new("r", "", "", ""));
        let blank = s.blank_ids();
        assert_eq!(blank, vec![id("blank")]);
    }

    #[test]
    fn prune_orphans_drops_entries_not_in_live_set() {
        let mut s = L1Store::new();
        s.set(id("a"), L1Description::new("ra", "", "", ""));
        s.set(id("b"), L1Description::new("rb", "", "", ""));
        s.set(id("c"), L1Description::new("rc", "", "", ""));
        let live = [id("a"), id("c")];
        let removed = s.prune_orphans(live.iter());
        assert_eq!(removed, 1);
        assert!(!s.contains(&id("b")));
        assert!(s.contains(&id("a")));
        assert!(s.contains(&id("c")));
    }

    #[test]
    fn store_round_trips_through_json() {
        let mut s = L1Store::new();
        s.set(id("a"), L1Description::new("r", "i", "d", "c").with_confidence(0.7));
        let j = serde_json::to_string(&s).unwrap();
        let restored: L1Store = serde_json::from_str(&j).unwrap();
        assert_eq!(restored.len(), 1);
        let d = restored.get(&id("a")).unwrap();
        assert_eq!(d.responsibility, "r");
        assert!((d.confidence - 0.7).abs() < 1e-9);
    }
}
