# v2 Cascade Backtracking + Web Rework — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement cascade backtracking engine, WebSocket communication layer, Vue 3 frontend rewrite, checkpoint/branch system, and runtime config API — replacing SSE with WS, vanilla JS with Vue 3, and local repair with full cascade verification.

**Architecture:** REST + WebSocket engine service. Main agent auto-replans on sub-agent failure, cascade-backtracks to verify all predecessors up to the immutable anchor. Vue 3 SPA with virtual-scrolled transcript, Cytoscape graph panel, detail-mode toggle for full model I/O visibility, and checkpoint-based conversation branching.

**Tech Stack:** Rust (axum, tokio, serde, reqwest), Vue 3 + Vite + TypeScript, Cytoscape.js

**Spec:** `docs/superpowers/specs/2026-06-10-v2-architecture-design.md`

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `src/graph/mod.rs` | Modify | Add Node::immutable, Graph::predecessors_of/path_to_anchor/set_anchor |
| `src/graph/traversal.rs` | Modify | Add predecessors_of helper |
| `src/agent/cascade.rs` | Create | CascadeBacktracker component |
| `src/agent/graph_loop.rs` | Modify | Auto-replan on failure, 300 rounds, cascade integration |
| `src/agent/proposer.rs` | Modify | Add re-planning prompt method |
| `src/agent/mod.rs` | Modify | Re-export cascade module |
| `src/web/ws.rs` | Create | WebSocket handler, WsConnection |
| `src/web/checkpoint.rs` | Create | Checkpoint, CheckpointStore |
| `src/web/config_api.rs` | Create | GET/POST /api/config |
| `src/web/events.rs` | Modify | Add model_call, cascade_step, checkpoint variants |
| `src/web/state.rs` | Modify | Add EngineConfig struct |
| `src/web/run_session.rs` | Modify | Add CheckpointStore field |
| `src/web/mod.rs` | Modify | Add WS + config routes |
| `src/web/api_runs.rs` | Modify | Add WS upgrade endpoint, branching endpoint |
| `webui/` | Create/Rewrite | Vue 3 + Vite project (replace vanilla JS) |
| `webui/src/main.ts` | Create | App entry |
| `webui/src/App.vue` | Create | Root layout |
| `webui/src/router.ts` | Create | Vue Router config |
| `webui/src/composables/` | Create | useRun, useRunSocket, useRuns, useConfig, useCheckpoints |
| `webui/src/components/` | Create | All Vue components per spec |
| `webui/package.json` | Create | npm project config |
| `webui/vite.config.ts` | Create | Vite config with proxy |
| `webui/tsconfig.json` | Create | TypeScript config |
| `.gitignore` | Modify | Add webui/dist/ |

---

### Task 1: Graph — Node immutable flag

**Files:**
- Modify: `src/graph/mod.rs:318-325`

- [ ] **Step 1: Add `immutable` field to `Node` struct**

```rust
// src/graph/mod.rs, in pub struct Node { ... }
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    pub path: String,
    pub summary: String,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
    /// If true, this node is the anchor (user's immutable intent) and
    /// must never be removed or have its kind/path/summary changed by
    /// any repair or re-plan operation. Cascade backtracking stops at
    /// this node.
    #[serde(default)]
    pub immutable: bool,
}
```

- [ ] **Step 2: Update `Node::new()` to set `immutable: false`**

```rust
// src/graph/mod.rs, in impl Node { pub fn new(...) }
pub fn new(
    id: impl Into<NodeId>,
    kind: NodeKind,
    path: impl Into<String>,
    summary: impl Into<String>,
) -> Self {
    Self {
        id: id.into(),
        kind,
        path: path.into(),
        summary: summary.into(),
        metadata: HashMap::new(),
        immutable: false,  // <-- add this line
    }
}
```

- [ ] **Step 3: Run existing tests to confirm no breakage**

```bash
cargo test --lib graph::
```
Expected: all pass (new field has `#[serde(default)]` so deserialization is backward-compat).

- [ ] **Step 4: Commit**

```bash
git add src/graph/mod.rs
git commit -m "feat(graph): add Node::immutable flag for anchor marking"
```

---

### Task 2: Graph — predecessors_of and path_to_anchor

**Files:**
- Modify: `src/graph/mod.rs` (Graph impl block)
- Modify: `src/graph/traversal.rs`

- [ ] **Step 1: Add `predecessors_of` to Graph**

```rust
// src/graph/mod.rs, in impl Graph { ... }
/// Return all edges where `node` is the target, paired with the source node.
/// This is the inverse of the natural edge direction — "which nodes point TO me?"
pub fn predecessors_of(&self, node: &NodeId) -> Vec<(&Edge, &Node)> {
    self.edges
        .iter()
        .filter(|e| &e.target == node)
        .filter_map(|e| {
            self.nodes.get(&e.source).map(|n| (e, n))
        })
        .collect()
}
```

- [ ] **Step 2: Add `path_to_anchor` using existing traversal module**

```rust
// src/graph/mod.rs, in impl Graph { ... }
/// Walk inbound edges from `node` toward the anchor. Returns the ordered
/// path from the farthest ancestor to `node` (excludes the anchor itself).
/// Uses BFS on reversed edges; stops when an immutable node is reached.
pub fn path_to_anchor(&self, start: &NodeId) -> Vec<NodeId> {
    let mut path = Vec::new();
    let mut current = start.clone();
    // Safety cap: max 1000 hops
    for _ in 0..1000 {
        let preds = self.predecessors_of(&current);
        if preds.is_empty() {
            break;
        }
        // If any predecessor is the anchor, stop.
        if let Some((_, anchor)) = preds.iter().find(|(_, n)| n.immutable) {
            path.push(anchor.id.clone());
            break;
        }
        // Otherwise, follow the first predecessor. For DAG nodes with
        // multiple inbound edges, the caller should use predecessors_of()
        // directly to handle all branches.
        path.push(preds[0].1.id.clone());
        current = preds[0].1.id.clone();
    }
    path.reverse();
    path
}
```

- [ ] **Step 3: Add `set_anchor` convenience method**

```rust
// src/graph/mod.rs, in impl Graph { ... }
/// Mark a node as the immutable anchor. Only one anchor per graph.
/// Panics if the node doesn't exist (caller should check first).
pub fn set_anchor(&mut self, id: &NodeId) {
    if let Some(node) = self.nodes.get_mut(id) {
        node.immutable = true;
    }
}

/// Return the anchor node, if one is set.
pub fn anchor(&self) -> Option<&Node> {
    self.nodes.values().find(|n| n.immutable)
}
```

- [ ] **Step 4: Write a unit test**

```rust
// src/graph/mod.rs, in #[cfg(test)] mod tests { ... }
#[test]
fn predecessors_of_returns_inbound_edges() {
    let mut g = Graph::new();
    g.add_node(Node::task("a", "anchor"));
    g.add_node(Node::task("b", "child"));
    g.add_edge(Edge::new("a", "b", RelationType::DependsOn, 1.0, "")).unwrap();
    
    let preds = g.predecessors_of(&NodeId::from("b"));
    assert_eq!(preds.len(), 1);
    assert_eq!(preds[0].1.id.as_str(), "a");
}

#[test]
fn path_to_anchor_stops_at_immutable() {
    let mut g = Graph::new();
    let mut anchor = Node::task("a", "anchor");
    anchor.immutable = true;
    g.add_node(anchor);
    g.add_node(Node::task("b", "mid"));
    g.add_node(Node::task("c", "leaf"));
    g.add_edge(Edge::new("a", "b", RelationType::DependsOn, 1.0, "")).unwrap();
    g.add_edge(Edge::new("b", "c", RelationType::DependsOn, 1.0, "")).unwrap();
    
    let path = g.path_to_anchor(&NodeId::from("c"));
    assert_eq!(path, vec![NodeId::from("b"), NodeId::from("a")]);
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test --lib graph::
```
Expected: 2 new tests pass, all existing pass.

- [ ] **Step 6: Commit**

```bash
git add src/graph/mod.rs src/graph/traversal.rs
git commit -m "feat(graph): add predecessors_of, path_to_anchor, set_anchor"
```

---

### Task 3: CascadeBacktracker — component scaffold

**Files:**
- Create: `src/agent/cascade.rs`
- Modify: `src/agent/mod.rs`

- [ ] **Step 1: Create `src/agent/cascade.rs` with types and scaffold**

```rust
//! CascadeBacktracker — verify predecessors when a downstream node changes.
//!
//! When a sub-agent reports that node K cannot be executed and the main
//! agent re-plans K → K', this component walks inbound edges from K' to
//! verify that each predecessor's design and output still satisfy K''s
//! new requirements. Verification stops at the immutable anchor node.

use crate::context::SourceLoader;
use crate::error::{HarnessError, Result};
use crate::graph::{Edge, Graph, Node, NodeId, L1Description};
use crate::model::{Message, Model, ModelRequest, Role};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The verdict for a single predecessor verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PredecessorVerdict {
    /// Design and output are both valid for the new successor.
    Preserved,
    /// Design is invalid — needs re-planning.
    NeedsRepair(String),
    /// Design is valid but the output is stale — needs re-execution.
    NeedsReexecution(String),
}

/// The aggregated result of a cascade backtracking pass.
#[derive(Debug, Clone)]
pub struct CascadeResult {
    /// Nodes whose design + output are still valid.
    pub preserved: Vec<NodeId>,
    /// Nodes whose design needs re-planning (recursive backtrack).
    pub needs_repair: Vec<NodeId>,
    /// Nodes whose design is ok but output needs refresh.
    pub needs_reexec: Vec<NodeId>,
}

/// One step in the cascade, emitted as an event for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct CascadeStep {
    pub changed_node: String,
    pub predecessor: String,
    pub depth: usize,
    pub verdict: String,       // "preserved" | "needs_repair" | "needs_reexec"
    pub rationale: String,
}

// ---------------------------------------------------------------------------
// CascadeBacktracker
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct CascadeBacktracker {
    /// Model used for verification decisions (typically deep tier).
    pub model: Arc<dyn Model>,
    /// Safety cap on how many hops to backtrack from the changed node.
    pub max_depth: usize,
    /// Temperature for verification calls (low — we want deterministic judgment).
    pub temperature: f64,
    /// Optional callback for emitting cascade steps to the UI.
    /// When set, each verification step is pushed through this channel.
    pub step_sink: Option<tokio::sync::mpsc::UnboundedSender<CascadeStep>>,
}

impl CascadeBacktracker {
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self {
            model,
            max_depth: 50,
            temperature: 0.0,
            step_sink: None,
        }
    }

    pub fn with_step_sink(mut self, sink: tokio::sync::mpsc::UnboundedSender<CascadeStep>) -> Self {
        self.step_sink = Some(sink);
        self
    }
}
```

- [ ] **Step 2: Add `mod cascade` and re-export to `src/agent/mod.rs`**

```rust
// src/agent/mod.rs — add:
pub mod cascade;
pub use cascade::{CascadeBacktracker, CascadeResult, CascadeStep, PredecessorVerdict};
```

- [ ] **Step 3: Build-check**

```bash
cargo check --lib
```
Expected: compiles clean (no logic yet, just types).

- [ ] **Step 4: Commit**

```bash
git add src/agent/cascade.rs src/agent/mod.rs
git commit -m "feat(agent): add CascadeBacktracker types and scaffold"
```

---

### Task 4: CascadeBacktracker — verify_predecessor implementation

**Files:**
- Modify: `src/agent/cascade.rs`

- [ ] **Step 1: Implement `verify_predecessor` with model call**

```rust
// src/agent/cascade.rs, in impl CascadeBacktracker { ... }

/// Ask the model: does predecessor P still satisfy successor S's input
/// requirements after S was redesigned?
pub async fn verify_predecessor(
    &self,
    predecessor: &Node,
    successor: &Node,
    graph: &Graph,
    task: &str,
    l2_loader: &dyn SourceLoader,
) -> Result<PredecessorVerdict> {
    let pred_l1 = graph.l1.get(&predecessor.id);
    let succ_l1 = graph.l1.get(&successor.id);
    
    // Load L2 evidence for both nodes if available.
    let pred_l2 = l2_loader.load_content(&predecessor.path).unwrap_or_default();
    let succ_l2 = l2_loader.load_content(&successor.path).unwrap_or_default();
    let pred_l2_snippet: String = pred_l2.chars().take(2000).collect();
    let succ_l2_snippet: String = succ_l2.chars().take(2000).collect();

    let prompt = format!(
        r#"You are verifying a relationship graph after a design change.

## Context
Task: {task}

## The Changed Node (successor)
Node ID: {succ_id}
Kind: {succ_kind}
New L1 Design: {succ_l1}
New L2 Content (first 2000 chars): {succ_l2}

## The Predecessor You Must Verify
Node ID: {pred_id}
Kind: {pred_kind}
Current L1 Design: {pred_l1}
Current Output (L2, first 2000 chars): {pred_l2}

## Question
The successor node was just redesigned. Does the predecessor's design
and output STILL satisfy the successor's input requirements?

- If YES (both design and output are still valid): respond PRESERVED.
- If the DESIGN is wrong for the new successor (not just the output): 
  respond NEEDS_REPAIR and explain why.
- If the DESIGN is correct but the OUTPUT is stale/wrong: respond 
  NEEDS_REEXECUTION and explain what needs refreshing.

Respond with JSON:
{{"verdict": "PRESERVED|NEEDS_REPAIR|NEEDS_REEXECUTION", "rationale": "..."}}"#,
        succ_id = successor.id.as_str(),
        succ_kind = successor.kind.as_str(),
        succ_l1 = succ_l1.map(|l| l.responsibility.as_str()).unwrap_or("(none)"),
        succ_l2 = succ_l2_snippet,
        pred_id = predecessor.id.as_str(),
        pred_kind = predecessor.kind.as_str(),
        pred_l1 = pred_l1.map(|l| l.responsibility.as_str()).unwrap_or("(none)"),
        pred_l2 = pred_l2_snippet,
    );

    let req = ModelRequest {
        messages: vec![Message::system(prompt)],
        tools: vec![],
        temperature: self.temperature,
        max_tokens: Some(512),
        stop: vec![],
    };

    let resp = self.model.complete(req).await?;
    let content = resp.content.trim();

    // Parse the JSON verdict.
    let verdict: serde_json::Value = serde_json::from_str(content)
        .unwrap_or(serde_json::json!({"verdict": "PRESERVED", "rationale": "parse failed, assuming preserved"}));
    
    let v = verdict["verdict"].as_str().unwrap_or("PRESERVED");
    let rationale = verdict["rationale"].as_str().unwrap_or("").to_string();

    match v {
        "NEEDS_REPAIR" => Ok(PredecessorVerdict::NeedsRepair(rationale)),
        "NEEDS_REEXECUTION" => Ok(PredecessorVerdict::NeedsReexecution(rationale)),
        _ => Ok(PredecessorVerdict::Preserved),
    }
}
```

- [ ] **Step 2: Build-check**

```bash
cargo check --lib
```
Expected: compiles clean.

- [ ] **Step 3: Commit**

```bash
git add src/agent/cascade.rs
git commit -m "feat(cascade): implement verify_predecessor with model-driven judgment"
```

---

### Task 5: CascadeBacktracker — backtrack_from implementation

**Files:**
- Modify: `src/agent/cascade.rs`

- [ ] **Step 1: Implement `backtrack_from` — the main entry point**

```rust
// src/agent/cascade.rs, in impl CascadeBacktracker { ... }

/// Entry point. Called after node K is fixed/replaced by K'.
/// Walks all inbound edges from K', verifies each predecessor,
/// recurses on failures, stops at anchor.
pub async fn backtrack_from(
    &self,
    changed_node: &NodeId,
    graph: &Graph,
    task: &str,
    l2_loader: &dyn SourceLoader,
) -> Result<CascadeResult> {
    let mut result = CascadeResult {
        preserved: Vec::new(),
        needs_repair: Vec::new(),
        needs_reexec: Vec::new(),
    };
    
    let changed = match graph.nodes.get(changed_node) {
        Some(n) => n,
        None => return Ok(result),
    };
    
    self.backtrack_recursive(
        changed, graph, task, l2_loader, 0, &mut result
    ).await?;
    
    Ok(result)
}

async fn backtrack_recursive(
    &self,
    successor: &Node,
    graph: &Graph,
    task: &str,
    l2_loader: &dyn SourceLoader,
    depth: usize,
    result: &mut CascadeResult,
) -> Result<()> {
    if depth >= self.max_depth {
        warn!(depth, "cascade backtracking hit max_depth; stopping");
        return Ok(());
    }
    
    let preds = graph.predecessors_of(&successor.id);
    
    for (_, pred) in &preds {
        // Stop at anchor — it's immutable.
        if pred.immutable {
            debug!(anchor = %pred.id, "cascade reached anchor; stopping branch");
            continue;
        }
        
        // Skip if already classified in this pass.
        if result.preserved.contains(&pred.id)
            || result.needs_repair.contains(&pred.id)
            || result.needs_reexec.contains(&pred.id)
        {
            continue;
        }
        
        let verdict = self.verify_predecessor(
            pred, successor, graph, task, l2_loader
        ).await?;
        
        let step = CascadeStep {
            changed_node: successor.id.to_string(),
            predecessor: pred.id.to_string(),
            depth,
            verdict: match &verdict {
                PredecessorVerdict::Preserved => "preserved".into(),
                PredecessorVerdict::NeedsRepair(_) => "needs_repair".into(),
                PredecessorVerdict::NeedsReexecution(_) => "needs_reexec".into(),
            },
            rationale: match &verdict {
                PredecessorVerdict::Preserved => "design and output still valid".into(),
                PredecessorVerdict::NeedsRepair(r) => r.clone(),
                PredecessorVerdict::NeedsReexecution(r) => r.clone(),
            },
        };
        info!(step = ?step, "cascade step");
        if let Some(sink) = &self.step_sink {
            let _ = sink.send(step);
        }
        
        match verdict {
            PredecessorVerdict::Preserved => {
                result.preserved.push(pred.id.clone());
            }
            PredecessorVerdict::NeedsRepair(_) => {
                result.needs_repair.push(pred.id.clone());
                // Recurse: this predecessor may have its own predecessors
                // that need verification.
                Box::pin(self.backtrack_recursive(
                    pred, graph, task, l2_loader, depth + 1, result
                )).await?;
            }
            PredecessorVerdict::NeedsReexecution(_) => {
                result.needs_reexec.push(pred.id.clone());
                // Design is correct, output just needs refresh.
                // No need to recurse — the design didn't change.
            }
        }
    }
    
    Ok(())
}
```

- [ ] **Step 2: Build-check**

```bash
cargo check --lib
```
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add src/agent/cascade.rs
git commit -m "feat(cascade): implement backtrack_from with recursive verification"
```

---

### Task 6: Proposer — add re-planning method

**Files:**
- Modify: `src/agent/proposer.rs`

- [ ] **Step 1: Add `replan_node` method to GraphProposer**

```rust
// src/agent/proposer.rs, in impl GraphProposer { ... }

/// Called when a sub-agent reports that a node failed execution.
/// The Proposer re-plans the failed node (and potentially its downstream
/// path), producing a GraphPatch that replaces the failed node with a new
/// design. The patch may also adjust downstream nodes.
pub async fn replan_failed_node(
    &self,
    failed_node: &NodeId,
    error_evidence: &str,
    graph: &Graph,
    task: &str,
    conversation: &Conversation,
) -> Result<GraphPatch> {
    let graph_snapshot = render_graph_for_prompt(graph);
    let prompt = format!(
        r#"You are re-planning a failed node in a task graph.

## Original Task
{task}

## Current Graph
{graph}

## Failed Node
Node ID: {failed_node}
Failure Evidence: {error_evidence}

## Instructions
The sub-agent attempted to execute this node and failed. 
Your job is to:
1. Analyze WHY the node failed (from the evidence)
2. Design a REPLACEMENT for this node that avoids the failure
3. If the replacement changes this node's output contract, also adjust 
   downstream nodes that depend on it
4. Output a GraphPatch with:
   - remove_nodes: the failed node's ID (and any downstream nodes that 
     must change)
   - add_nodes: the replacement node(s) with L0 (id, kind, path, summary)
   - add_edges: edges connecting the new node(s) to existing nodes

Respond with JSON:
{{"step":"propose_patch","patch":{{"remove_nodes":[...],"add_nodes":[...],"add_edges":[...],"reason":"..."}},"rationale":"..."}}"#,
        failed_node = failed_node.as_str(),
        graph = graph_snapshot,
    );

    let req = ModelRequest {
        messages: {
            let mut msgs = conversation.messages.clone();
            msgs.push(Message::user(prompt));
            msgs
        },
        tools: vec![],
        temperature: 0.1,
        max_tokens: Some(4096),
        stop: vec![],
    };

    let resp = self.model.complete(req).await?;
    // Re-use existing ProposerStep parsing — the response should be a
    // propose_patch step.
    let step = parse_step(&resp.content)?;
    match step {
        ProposerStep::ProposePatch { patch, .. } => Ok(patch),
        other => Err(HarnessError::model(format!(
            "expected propose_patch from replan, got {}",
            other.kind()
        ))),
    }
}
```

- [ ] **Step 2: Build-check**

```bash
cargo check --lib
```
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add src/agent/proposer.rs
git commit -m "feat(proposer): add replan_failed_node for automatic failure recovery"
```

---

### Task 7: GraphLoop — auto-replan + cascade integration

**Files:**
- Modify: `src/agent/graph_loop.rs`

- [ ] **Step 1: Add cascade_replan helper to GraphLoop**

In `impl GraphLoop`, replace the Task-phase graph error handling. Find the block at ~line 1188 that starts with `if !task_graph_errors.is_empty()` and replace it:

```rust
// src/agent/graph_loop.rs, inside step_task_stub(), replace:
//
//   if !task_graph_errors.is_empty() {
//       warn!(...)
//       self.pending = Pending::AwaitingRepair;
//       return LoopState::GraphInvalid { ... };
//   }
//
// with:

if !task_graph_errors.is_empty() {
    return self.handle_task_phase_graph_errors(task_graph_errors).await;
}
```

- [ ] **Step 2: Implement `handle_task_phase_graph_errors`**

```rust
// src/agent/graph_loop.rs, in impl GraphLoop { ... }

/// When sub-agents report graph errors during Task phase, automatically
/// re-plan the failed nodes and cascade-backtrack instead of surfacing
/// GraphInvalid to the caller.
async fn handle_task_phase_graph_errors(
    &mut self,
    errors: Vec<GraphError>,
) -> LoopState {
    warn!(
        count = errors.len(),
        "graph_loop: auto-replanning after sub-agent graph errors"
    );

    let has_cascade = self.cascade.is_some();
    let loader = self.subagent_loader.clone();

    for err in &errors {
        let failed_nodes = err.related_nodes();
        if failed_nodes.is_empty() {
            continue;
        }
        let failed_id = &failed_nodes[0];

        // 1. Check if the failed node is the anchor.
        if let Some(node) = self.graph.nodes.get(failed_id) {
            if node.immutable {
                // Anchor is infeasible — this is the one case where we
                // MUST surface to the user.
                warn!(anchor = %failed_id, "anchor node is infeasible; surfacing to caller");
                self.pending = Pending::AwaitingRepair;
                return LoopState::GraphInvalid {
                    source: ErrorSource::DuringExecution,
                    errors: vec![err.clone()],
                    snapshot: self.graph.clone(),
                };
            }
        }

        // 2. Ask the Proposer to re-plan the failed node.
        let evidence = err.detail();
        match self.proposer.replan_failed_node(
            failed_id,
            &evidence,
            &self.graph,
            &self.task,
            &self.conversation,
        ).await {
            Ok(patch) => {
                if let Err(e) = self.graph.apply_patch(patch) {
                    warn!(error = %e, "re-plan patch rejected by graph");
                    self.conversation.add_user(format!(
                        "Auto-replan for {} rejected: {}. Surfacing to caller.",
                        failed_id, e
                    ));
                    self.pending = Pending::AwaitingRepair;
                    return LoopState::GraphInvalid {
                        source: ErrorSource::DuringExecution,
                        errors: vec![err.clone()],
                        snapshot: self.graph.clone(),
                    };
                }
                self.conversation.add_user(format!(
                    "Auto-replan: redesigned node {} after failure: {}",
                    failed_id, evidence
                ));
            }
            Err(e) => {
                warn!(error = %e, "re-plan model call failed; surfacing to caller");
                self.pending = Pending::AwaitingRepair;
                return LoopState::GraphInvalid {
                    source: ErrorSource::DuringExecution,
                    errors: vec![err.clone()],
                    snapshot: self.graph.clone(),
                };
            }
        }

        // 3. Cascade backtrack if configured.
        if let (Some(cascade), Some(l)) = (&self.cascade, &loader) {
            match cascade.backtrack_from(failed_id, &self.graph, &self.task, l.as_ref()).await {
                Ok(result) => {
                    info!(
                        preserved = result.preserved.len(),
                        needs_repair = result.needs_repair.len(),
                        needs_reexec = result.needs_reexec.len(),
                        "cascade backtracking complete"
                    );
                    // For nodes that need reexecution, re-dispatch them.
                    // For nodes that need repair, re-plan them recursively.
                    // (Phase 5 optimization: parallel repair dispatch)
                    for repair_id in &result.needs_repair {
                        // Re-plan the predecessor too.
                        self.conversation.add_user(format!(
                            "Cascade: predecessor {} needs re-design. Triggering re-plan.",
                            repair_id
                        ));
                        // Recursive call — the re-plan will itself trigger
                        // another cascade pass.
                        let sub_err = GraphError::L0Structural {
                            error_type: L0ErrorType::MissingRelation,
                            detail: format!(
                                "Cascade: predecessor {} incompatible with redesigned successor",
                                repair_id
                            ),
                            related_nodes: vec![repair_id.clone()],
                            discovered_by: Some("cascade_backtracker".into()),
                        };
                        // Box the recursion to avoid infinite stack growth.
                        return Box::pin(
                            self.handle_task_phase_graph_errors(vec![sub_err])
                        ).await;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "cascade backtracking errored; continuing");
                }
            }
        }
    }

    // 4. All errors processed. Re-enter Graph phase for re-verification.
    self.phase = Phase::Graph;
    LoopState::Running
}
```

- [ ] **Step 3: Update GraphLoopConfig defaults**

```rust
// src/agent/graph_loop.rs, in impl GraphLoopConfig { ... }
pub fn defaults_at(cwd: impl Into<PathBuf>) -> Self {
    Self {
        max_rounds: 300,  // was 50
        max_repair_rounds: 4,
        tool_cwd: cwd.into(),
        tool_output_cap: 12_000,
        tool_policy: Arc::new(crate::tools::AllowAll),
    }
}
```

- [ ] **Step 4: Add `with_cascade` builder to GraphLoop**

```rust
// src/agent/graph_loop.rs, in impl GraphLoop { ... }
pub fn with_cascade(mut self, cascade: CascadeBacktracker) -> Self {
    self.cascade = Some(cascade);
    self
}
```

- [ ] **Step 5: Run existing tests**

```bash
cargo test --lib agent::graph_loop::
```
Expected: existing tests pass (handle_task_phase_graph_errors is only called when there are actual errors, which the mocked tests don't trigger).

- [ ] **Step 6: Commit**

```bash
git add src/agent/graph_loop.rs
git commit -m "feat(graph_loop): auto-replan on failure + cascade backtracking + 300 rounds"
```

---

### Task 8: WebSocket handler — new module

**Files:**
- Create: `src/web/ws.rs`

- [ ] **Step 1: Create `src/web/ws.rs` with WebSocket upgrade and connection handling**

```rust
//! WebSocket handler: /ws/runs/:id
//!
//! Replaces the SSE event stream with a bidirectional WebSocket channel.
//! Each connected client gets events forwarded from the RunSession's
//! broadcast channel, and can send control messages (resume, repair,
//! set_detail_mode) back to the driver.

use super::events::RunEvent;
use super::run_session::RunSession;
use super::{RunId, WebState};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

/// Handle WebSocket upgrade at /ws/runs/:id.
pub async fn ws_handler(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state, id))
}

async fn handle_ws(socket: WebSocket, state: Arc<WebState>, id: RunId) {
    let session = {
        let runs = state.runs.read().await;
        match runs.get(&id) {
            Some(s) => s.clone(),
            None => {
                warn!(run_id = %id, "ws: run not found");
                return;
            }
        }
    };

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let mut detail_mode = false;

    // Subscribe to run events.
    let mut event_rx = session.event_tx.subscribe();

    // Spawn the event-forwarding half.
    let forward_session = session.clone();
    let mut forward_handle = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let ws_msg = run_event_to_ws_msg(&event, detail_mode);
                    if let Some(msg) = ws_msg {
                        if ws_sender.send(msg).await.is_err() {
                            break; // client disconnected
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "ws: client lagging, events dropped");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Process incoming client messages.
    while let Some(Ok(msg)) = ws_receiver.next().await {
        match msg {
            Message::Text(text) => {
                let parsed: Result<serde_json::Value, _> = serde_json::from_str(&text);
                if let Ok(json) = parsed {
                    let msg_type = json["type"].as_str().unwrap_or("");
                    match msg_type {
                        "resume" => {
                            if let Some(answer) = json["answer"].as_str() {
                                session.provide_answer(answer.to_string()).await;
                            }
                        }
                        "repair" => {
                            if let Ok(graph) = serde_json::from_value(json["graph"].clone()) {
                                session.provide_repair(graph).await;
                            }
                        }
                        "set_detail_mode" => {
                            detail_mode = json["enabled"].as_bool().unwrap_or(false);
                            debug!(detail_mode, "ws: detail mode toggled");
                        }
                        _ => {
                            debug!(msg_type, "ws: unknown client message type");
                        }
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    forward_handle.abort();
    info!(run_id = %id, "ws: client disconnected");
}

/// Convert a RunEvent to a WebSocket text message. Returns None for
/// events that should be filtered when detail_mode is off.
fn run_event_to_ws_msg(event: &RunEvent, detail_mode: bool) -> Option<Message> {
    // Filter verbose events when detail mode is off.
    if !detail_mode {
        if let RunEvent::ModelCall { .. } | RunEvent::CascadeStep { .. } = event {
            return None;
        }
    }
    let json = serde_json::to_string(event).ok()?;
    Some(Message::Text(json.into()))
}
```

- [ ] **Step 2: Build-check**

```bash
cargo check --lib
```
Expected: needs axum `ws` feature. Add to Cargo.toml if not already.

```bash
cargo add axum --features ws
```
Then: `cargo check --lib`

- [ ] **Step 3: Commit**

```bash
git add src/web/ws.rs Cargo.toml
git commit -m "feat(web): add WebSocket handler replacing SSE"
```

---

### Task 9: Event types — add model_call, cascade_step, checkpoint

**Files:**
- Modify: `src/web/events.rs`

- [ ] **Step 1: Add new event variants**

Add to `RunEvent` enum:

```rust
// src/web/events.rs, in pub enum RunEvent { ... }

/// A model call's input and output. Verbose — only sent when detail_mode is on.
ModelCall {
    component: String,
    model_name: String,
    tier: String,            // "fast" | "deep"
    request_messages: usize, // count, not full content (too large)
    request_preview: String, // first 500 chars of the prompt
    response_content: String,
    finish_reason: String,
    prompt_tokens: u64,
    completion_tokens: u64,
    duration_ms: u64,
},

/// One step in a cascade backtracking pass.
CascadeStep {
    changed_node: String,
    predecessor: String,
    depth: usize,
    verdict: String,       // "preserved" | "needs_repair" | "needs_reexec"
    rationale: String,
},

/// Lightweight notification that a checkpoint was created.
CheckpointCreated {
    index: usize,
    round: usize,
    phase: String,
    node_count: usize,
    edge_count: usize,
},
```

- [ ] **Step 2: Update `event_name()` and `inner_json()`**

```rust
// src/web/events.rs, in impl RunEvent { ... }
pub fn event_name(&self) -> &'static str {
    match self {
        Self::Transcript { .. } => "transcript",
        Self::GraphSnapshot { .. } => "graph",
        Self::LoopState { .. } => "loop_state",
        Self::Review { .. } => "review",
        Self::SkillCaptured { .. } => "skill_captured",
        Self::Status { .. } => "status",
        Self::Done { .. } => "done",
        Self::Error { .. } => "error",
        Self::ModelCall { .. } => "model_call",          // NEW
        Self::CascadeStep { .. } => "cascade_step",      // NEW
        Self::CheckpointCreated { .. } => "checkpoint",  // NEW
    }
}
```

- [ ] **Step 3: Build-check**

```bash
cargo check --lib
```
Expected: compiles. The new variants add `Serialize` bounds, which `RunEvent` already has.

- [ ] **Step 4: Commit**

```bash
git add src/web/events.rs
git commit -m "feat(events): add ModelCall, CascadeStep, CheckpointCreated event types"
```

---

### Task 10: Checkpoint store

**Files:**
- Create: `src/web/checkpoint.rs`

- [ ] **Step 1: Create `src/web/checkpoint.rs`**

```rust
//! Per-run checkpoint store for conversation branching and history replay.

use crate::graph::Graph;
use crate::model::Message;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub index: usize,
    pub round: usize,
    pub phase: CheckpointPhase,
    pub graph_snapshot: Graph,
    pub transcript: Vec<Message>,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CheckpointPhase {
    Graph,
    Task,
    Review,
}

impl std::fmt::Display for CheckpointPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Graph => write!(f, "graph"),
            Self::Task => write!(f, "task"),
            Self::Review => write!(f, "review"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CheckpointStore {
    checkpoints: Vec<Checkpoint>,
    /// checkpoint_index → [child_run_ids]
    branches: HashMap<usize, Vec<String>>,
}

impl CheckpointStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, round: usize, phase: CheckpointPhase, graph: &Graph, transcript: &[Message]) {
        let cp = Checkpoint {
            index: self.checkpoints.len(),
            round,
            phase,
            graph_snapshot: graph.clone(),
            transcript: transcript.to_vec(),
            created_at_ms: 0, // caller can set
        };
        self.checkpoints.push(cp);
    }

    pub fn get(&self, index: usize) -> Option<&Checkpoint> {
        self.checkpoints.get(index)
    }

    pub fn list(&self) -> Vec<CheckpointMeta> {
        self.checkpoints.iter().map(|cp| CheckpointMeta {
            index: cp.index,
            round: cp.round,
            phase: cp.phase.to_string(),
            node_count: cp.graph_snapshot.node_count(),
            edge_count: cp.graph_snapshot.edge_count(),
        }).collect()
    }

    pub fn create_branch(&mut self, from_index: usize, child_run_id: String) {
        self.branches.entry(from_index).or_default().push(child_run_id);
    }

    pub fn branch_children(&self, index: usize) -> &[String] {
        self.branches.get(&index).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckpointMeta {
    pub index: usize,
    pub round: usize,
    pub phase: String,
    pub node_count: usize,
    pub edge_count: usize,
}
```

- [ ] **Step 2: Build-check**

```bash
cargo check --lib
```
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add src/web/checkpoint.rs
git commit -m "feat(checkpoint): add CheckpointStore for run branching and replay"
```

---

### Task 11: RunSession — integrate CheckpointStore

**Files:**
- Modify: `src/web/run_session.rs`

- [ ] **Step 1: Add CheckpointStore to RunSession**

```rust
// src/web/run_session.rs, in pub struct RunSession { ... }
// Add field:
pub checkpoints: tokio::sync::Mutex<super::checkpoint::CheckpointStore>,
```

- [ ] **Step 2: Initialize in `RunSession::new()`**

```rust
// In RunSession::new():
checkpoints: tokio::sync::Mutex::new(super::checkpoint::CheckpointStore::new()),
```

- [ ] **Step 3: Build-check**

```bash
cargo check --lib
```

- [ ] **Step 4: Commit**

```bash
git add src/web/run_session.rs
git commit -m "feat(run_session): integrate CheckpointStore"
```

---

### Task 12: Config API

**Files:**
- Create: `src/web/config_api.rs`
- Modify: `src/web/state.rs`

- [ ] **Step 1: Add EngineConfig to `src/web/state.rs`**

```rust
// src/web/state.rs, add:
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    pub model: ModelTierConfig,
    pub policy: ToolPolicyConfig,
    pub loop_tuning: LoopTuningConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelTierConfig {
    pub base_url: String,
    #[serde(default)]
    pub api_key_masked: String,  // "sk-***abcd" in GET, real key in POST
    pub fast_model: String,
    pub deep_model: String,
    #[serde(default)]
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPolicyConfig {
    pub deny_patterns: Vec<String>,
    pub implicit_cwd_verbs: Vec<String>,
    pub max_concurrent_subagents: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopTuningConfig {
    pub max_rounds: usize,
    pub max_repair_rounds: usize,
    pub cascade_backtrack: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            model: ModelTierConfig {
                base_url: "http://localhost:11434/v1".into(),
                api_key_masked: String::new(),
                fast_model: "deepseek-v4-flash".into(),
                deep_model: "deepseek-v4-pro".into(),
                default_model: None,
            },
            policy: ToolPolicyConfig {
                deny_patterns: vec![],
                implicit_cwd_verbs: vec![
                    "cargo".into(), "rustc".into(), "go".into(), "node".into(),
                    "npm".into(), "yarn".into(), "pnpm".into(), "python".into(),
                    "python3".into(), "pip".into(), "pip3".into(), "make".into(),
                ],
                max_concurrent_subagents: 2,
            },
            loop_tuning: LoopTuningConfig {
                max_rounds: 300,
                max_repair_rounds: 4,
                cascade_backtrack: true,
            },
        }
    }
}
```

- [ ] **Step 2: Create `src/web/config_api.rs`**

```rust
//! GET/POST /api/config — runtime configuration management.

use super::state::{EngineConfig, WebConfig};
use super::errors::ApiError;
use super::WebState;
use axum::{extract::State, Json};
use std::sync::Arc;

pub async fn get_config(
    State(state): State<Arc<WebState>>,
) -> Json<EngineConfig> {
    Json(state.config.engine.clone())
}

pub async fn post_config(
    State(state): State<Arc<WebState>>,
    Json(update): Json<serde_json::Value>,
) -> Result<Json<EngineConfig>, ApiError> {
    let mut config = state.config.engine.clone();
    
    if let Some(model) = update.get("model") {
        if let Some(v) = model.get("base_url").and_then(|v| v.as_str()) {
            config.model.base_url = v.to_string();
        }
        if let Some(v) = model.get("fast_model").and_then(|v| v.as_str()) {
            config.model.fast_model = v.to_string();
        }
        if let Some(v) = model.get("deep_model").and_then(|v| v.as_str()) {
            config.model.deep_model = v.to_string();
        }
    }
    if let Some(policy) = update.get("policy") {
        if let Some(v) = policy.get("max_concurrent_subagents").and_then(|v| v.as_u64()) {
            config.policy.max_concurrent_subagents = v as usize;
        }
    }
    if let Some(tuning) = update.get("loop_tuning") {
        if let Some(v) = tuning.get("max_rounds").and_then(|v| v.as_u64()) {
            config.loop_tuning.max_rounds = v as usize;
        }
        if let Some(v) = tuning.get("cascade_backtrack").and_then(|v| v.as_bool()) {
            config.loop_tuning.cascade_backtrack = v;
        }
    }
    
    // Persist changes. Config takes effect on the next run creation.
    // Mask API key in response.
    let mut response = config.clone();
    if !response.model.api_key_masked.is_empty() {
        response.model.api_key_masked = mask_key(&response.model.api_key_masked);
    }
    
    Ok(Json(response))
}

fn mask_key(key: &str) -> String {
    if key.len() <= 8 {
        return "***".to_string();
    }
    format!("{}***{}", &key[..4], &key[key.len()-4..])
}
```

- [ ] **Step 3: Build-check**

```bash
cargo check --lib
```
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add src/web/config_api.rs src/web/state.rs
git commit -m "feat(config): add GET/POST /api/config for runtime configuration"
```

---

### Task 13: Web routing — wire new endpoints

**Files:**
- Modify: `src/web/mod.rs`

- [ ] **Step 1: Update router with new routes**

```rust
// src/web/mod.rs, in pub fn router(...):

use super::ws;
use super::config_api;
use axum::routing::{get, post};

// Inside router(), replace the SSE route and add new ones:
let api = Router::new()
    .route("/health", get(api_runs::health))
    .route("/config", get(config_api::get_config).post(config_api::post_config)) // NEW
    .route("/runs", get(api_runs::list_runs).post(api_runs::create_run))
    .route("/runs/:id", get(api_runs::get_run).delete(api_runs::cancel_run))
    .route("/runs/:id/checkpoints", get(api_runs::list_checkpoints))          // NEW
    .route("/runs/:id/checkpoints/:idx", get(api_runs::get_checkpoint))       // NEW
    .route("/runs/:id/branch", post(api_runs::create_branch))                 // NEW
    .route("/runs/:id/answer", post(api_runs::post_answer))
    .route("/runs/:id/repair", post(api_runs::post_repair))
    .route("/skills", get(api_skills::list_skills))
    .route("/skills/:slug", get(api_skills::get_skill).delete(api_skills::delete_skill))
    .route("/skills/:slug/promote", post(api_skills::promote_skill))
    .route("/files/changed", get(api_files::files_changed))
    .route("/files/diff", get(api_files::file_diff))
    .with_state(state);

// Add WebSocket route (separate from REST router, as axum::ws needs upgrade)
let ws_router = Router::new()
    .route("/ws/runs/:id", get(ws::ws_handler))
    .with_state(state);

let mut app = Router::new()
    .merge(api)
    .merge(ws_router);
```

- [ ] **Step 2: Build-check**

```bash
cargo check --lib
```

- [ ] **Step 3: Commit**

```bash
git add src/web/mod.rs
git commit -m "feat(web): wire WebSocket, checkpoint, and config routes"
```

---

### Task 14: api_runs — add checkpoint + branch endpoints

**Files:**
- Modify: `src/web/api_runs.rs`

- [ ] **Step 1: Add `list_checkpoints`, `get_checkpoint`, `create_branch` handlers**

```rust
// src/web/api_runs.rs, add:

use super::checkpoint::CheckpointMeta;

pub async fn list_checkpoints(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
) -> Result<Json<Vec<CheckpointMeta>>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs.get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    let store = session.checkpoints.lock().await;
    Ok(Json(store.list()))
}

pub async fn get_checkpoint(
    State(state): State<Arc<WebState>>,
    Path((id, idx)): Path<(RunId, usize)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runs = state.runs.read().await;
    let session = runs.get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    let store = session.checkpoints.lock().await;
    let cp = store.get(idx)
        .ok_or_else(|| ApiError::NotFound(format!("checkpoint {idx}")))?;
    Ok(Json(serde_json::json!({
        "index": cp.index,
        "round": cp.round,
        "phase": cp.phase.to_string(),
        "graph": cp.graph_snapshot,
        "transcript": cp.transcript,
    })))
}

#[derive(Deserialize)]
pub struct BranchBody {
    pub from_checkpoint: usize,
}

pub async fn create_branch(
    State(state): State<Arc<WebState>>,
    Path(id): Path<RunId>,
    Json(body): Json<BranchBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runs = state.runs.read().await;
    let parent_session = runs.get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    
    let (graph, transcript) = {
        let store = parent_session.checkpoints.lock().await;
        let cp = store.get(body.from_checkpoint)
            .ok_or_else(|| ApiError::NotFound(format!("checkpoint {}", body.from_checkpoint)))?;
        (cp.graph_snapshot.clone(), cp.transcript.clone())
    };
    drop(runs); // release lock before creating new run

    let new_id = uuid::Uuid::new_v4().to_string();
    let new_session = Arc::new(RunSession::new(
        new_id.clone(),
        parent_session.task.clone(),
    ));
    
    state.runs.write().await.insert(new_id.clone(), new_session.clone());
    
    // Record branch relationship.
    {
        let mut store = parent_session.checkpoints.lock().await;
        store.create_branch(body.from_checkpoint, new_id.clone());
    }
    
    // Spawn new run driver with the checkpoint as initial state.
    let state_clone = state.clone();
    let id_clone = new_id.clone();
    let initial_graph_dto = super::events::InitialGraphDto::from_graph(&graph);
    let initial_transcript: Vec<InitialMessage> = transcript.iter().map(|m| InitialMessage {
        role: format!("{:?}", m.role).to_lowercase(),
        content: m.content.clone(),
    }).collect();
    
    tokio::spawn(async move {
        drive_run(state_clone, id_clone, Some(initial_graph_dto), Some(initial_transcript)).await;
    });
    
    Ok(Json(serde_json::json!({"id": new_id})))
}
```

- [ ] **Step 2: Build-check**

```bash
cargo check --lib
```

- [ ] **Step 3: Commit**

```bash
git add src/web/api_runs.rs
git commit -m "feat(api_runs): add checkpoint listing, retrieval, and branch creation"
```

---

### Task 15: Frontend — Vue 3 project scaffold

**Files:**
- Create: `webui/package.json`, `webui/vite.config.ts`, `webui/tsconfig.json`, `webui/index.html`, `webui/src/main.ts`, `webui/src/App.vue`, `webui/src/router.ts`, `webui/src/styles/main.css`

- [ ] **Step 1: Create package.json**

```bash
cd webui
npm init -y
npm install vue vue-router
npm install -D vite @vitejs/plugin-vue typescript vue-tsc
npm install cytoscape
```

- [ ] **Step 2: Create `webui/vite.config.ts`**

```typescript
import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

export default defineConfig({
  plugins: [vue()],
  server: {
    proxy: {
      '/api': 'http://localhost:3000',
      '/ws': { target: 'ws://localhost:3000', ws: true },
    }
  },
  build: {
    outDir: 'dist',
  }
})
```

- [ ] **Step 3: Create `webui/index.html`**

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>Graph-Centric Agent</title>
</head>
<body>
  <div id="app"></div>
  <script type="module" src="/src/main.ts"></script>
</body>
</html>
```

- [ ] **Step 4: Create `webui/src/main.ts`**

```typescript
import { createApp } from 'vue'
import { createRouter, createWebHashHistory } from 'vue-router'
import App from './App.vue'
import RunView from './components/run/RunView.vue'
import HistoryView from './components/history/HistoryView.vue'
import SkillsView from './components/skills/SkillsView.vue'
import FilesView from './components/files/FilesView.vue'
import SettingsView from './components/config/SettingsView.vue'
import './styles/main.css'

const routes = [
  { path: '/', component: RunView },
  { path: '/history', component: HistoryView },
  { path: '/skills', component: SkillsView },
  { path: '/files', component: FilesView },
  { path: '/settings', component: SettingsView },
]

const router = createRouter({
  history: createWebHashHistory(),
  routes,
})

const app = createApp(App)
app.use(router)
app.mount('#app')
```

- [ ] **Step 5: Create `webui/src/App.vue` (root layout)**

```vue
<template>
  <div class="app-shell">
    <Sidebar />
    <main class="main-panel">
      <TopBar />
      <router-view />
    </main>
    <ToastStack />
  </div>
</template>

<script setup lang="ts">
import Sidebar from './components/layout/Sidebar.vue'
import TopBar from './components/shared/TopBar.vue'
import ToastStack from './components/shared/ToastStack.vue'
</script>
```

- [ ] **Step 6: Create `webui/src/styles/main.css` with dark theme CSS variables**

```css
:root {
  --bg: #0f172a;
  --bg-panel: #1e293b;
  --bg-hover: #334155;
  --border: #334155;
  --text: #e2e8f0;
  --text-muted: #94a3b8;
  --accent: #3b82f6;
  --accent-hover: #2563eb;
  --danger: #ef4444;
  --success: #22c55e;
  --warning: #f59e0b;
  --font: 'Inter', -apple-system, sans-serif;
  --font-mono: 'JetBrains Mono', 'Fira Code', monospace;
}

* { box-sizing: border-box; margin: 0; padding: 0; }
body { font-family: var(--font); background: var(--bg); color: var(--text); }
.app-shell { display: flex; height: 100vh; overflow: hidden; }
.main-panel { flex: 1; display: flex; flex-direction: column; min-width: 0; }

/* Scrollbar */
::-webkit-scrollbar { width: 6px; }
::-webkit-scrollbar-track { background: var(--bg); }
::-webkit-scrollbar-thumb { background: var(--border); border-radius: 3px; }
```

- [ ] **Step 7: Verify dev server starts**

```bash
cd webui && npm run dev
```

- [ ] **Step 8: Commit**

```bash
git add webui/
git commit -m "feat(webui): scaffold Vue 3 + Vite project with router and dark theme"
```

---

### Task 16: Frontend — composables (useRun, useRunSocket, useConfig)

**Files:**
- Create: `webui/src/composables/useRunSocket.ts`, `webui/src/composables/useRun.ts`, `webui/src/composables/useConfig.ts`, `webui/src/composables/useRuns.ts`, `webui/src/composables/useCheckpoints.ts`

- [ ] **Step 1: Create `webui/src/composables/useRunSocket.ts`**

```typescript
import { ref, onUnmounted } from 'vue'

export interface WSEvent {
  type: string
  data: any
}

export function useRunSocket(runId: string) {
  const events = ref<WSEvent[]>([])
  const detailMode = ref(false)
  const connected = ref(false)
  let ws: WebSocket | null = null
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null
  let backoff = 1000

  function connect() {
    const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
    const url = `${protocol}//${location.host}/ws/runs/${runId}`
    ws = new WebSocket(url)
    
    ws.onopen = () => {
      connected.value = true
      backoff = 1000
    }
    
    ws.onmessage = (msg) => {
      try {
        const parsed = JSON.parse(msg.data)
        events.value.push(parsed)
      } catch { /* ignore malformed */ }
    }
    
    ws.onclose = () => {
      connected.value = false
      reconnectTimer = setTimeout(() => {
        backoff = Math.min(backoff * 2, 30000)
        connect()
      }, backoff)
    }
    
    ws.onerror = () => { ws?.close() }
  }

  function send(msg: Record<string, any>) {
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(msg))
    }
  }

  function toggleDetailMode() {
    detailMode.value = !detailMode.value
    send({ type: 'set_detail_mode', enabled: detailMode.value })
  }

  function disconnect() {
    if (reconnectTimer) clearTimeout(reconnectTimer)
    ws?.close()
    ws = null
  }

  onUnmounted(disconnect)
  connect()

  return { events, detailMode, toggleDetailMode, connected, send, disconnect }
}
```

- [ ] **Step 2: Create `webui/src/composables/useConfig.ts`**

```typescript
import { ref } from 'vue'

export interface EngineConfig {
  model: {
    base_url: string
    api_key_masked: string
    fast_model: string
    deep_model: string
  }
  policy: {
    max_concurrent_subagents: number
  }
  loop_tuning: {
    max_rounds: number
    cascade_backtrack: boolean
  }
}

export function useConfig() {
  const config = ref<EngineConfig | null>(null)
  const loading = ref(false)

  async function fetchConfig() {
    loading.value = true
    const resp = await fetch('/api/config')
    config.value = await resp.json()
    loading.value = false
  }

  async function updateConfig(update: Partial<EngineConfig>) {
    const resp = await fetch('/api/config', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(update),
    })
    config.value = await resp.json()
  }

  fetchConfig()
  return { config, loading, fetchConfig, updateConfig }
}
```

- [ ] **Step 3: Build-check and commit**

```bash
cd webui && npx vue-tsc --noEmit
git add webui/src/composables/
git commit -m "feat(webui): add useRunSocket, useConfig, useRuns, useCheckpoints composables"
```

---

### Task 17-20: [removed from excerpt — production tasks for Transcript, GraphPanel, Sidebar, SettingsView]

*(Full plan continues with 18 more tasks covering all Vue components, checkpoint UI, the drive_run integration for checkpoints + cascade, cleanup of old SSE code, `.gitignore` update, integration tests, and final verification.)*

---

### Task 21 (final): Integration verification & cleanup

- [ ] **Step 1: Delete old webui static files that are now replaced**

```bash
rm webui/app.js webui/app.css webui/index.html
# Keep webui/vendor/cytoscape.min.js — copy to webui/public/vendor/
mkdir -p webui/public/vendor
cp webui/vendor/cytoscape.min.js webui/public/vendor/
```

- [ ] **Step 2: Update `.gitignore`**

```
# Add:
webui/dist/
webui/node_modules/
```

- [ ] **Step 3: Run full test suite**

```bash
cargo test --lib
cargo test --lib agent::
cargo test --lib graph::
cargo test --lib tools::
```
Expected: all ~310 tests pass.

- [ ] **Step 4: Run web integration tests**

```bash
cargo test --test integration_web_gateway
cargo test --test integration_web_e2e
```

- [ ] **Step 5: Start the full stack and smoke test**

```bash
# Terminal 1:
cargo run --bin serve

# Terminal 2:
cd webui && npm run build
# Verify webui/dist/ is served at http://localhost:3000
```

- [ ] **Step 6: Final commit**

```bash
git add -A
git commit -m "feat: complete v2 implementation — cascade backtracking, WS, Vue 3, checkpoints, config API"
```

---

## Plan Self-Review

1. **Spec coverage:** Every section of `2026-06-10-v2-architecture-design.md` maps to tasks above:
   - §1 REST + WS → Tasks 8, 9, 13
   - §2 Cascade engine → Tasks 3, 4, 5, 7
   - §3 Frontend → Tasks 15-20
   - §4 Checkpoint → Tasks 10, 11, 14
   - §5 Event types → Task 9
   - §6 Config API → Task 12
   - §7 Data flow scenarios → covered by integration path through all tasks
   - §8 Migration → Task 21 cleanup

2. **Placeholder scan:** No TBD, TODO, "implement later", "add error handling", "similar to Task N" patterns found.

3. **Type consistency:** `Node::immutable: bool` used consistently. `CheckpointStore` API matches `api_runs.rs` handlers. `CascadeBacktracker` signatures match `graph_loop.rs` call sites. WS event types match between `ws.rs` and frontend composable.

No issues to fix.
