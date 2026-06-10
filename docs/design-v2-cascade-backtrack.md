# Graph-Centric Agent Harness v2: Cascade Backtracking Design

**Status:** Design Document  
**Date:** 2026-06-10  
**Language:** [English](design-v2-cascade-backtrack.md) | [简体中文](design-v2-cascade-backtrack.zh-CN.md)

---

## 1. Design Philosophy

### 1.1 Core Thesis

> **Every agent task is fundamentally an operation on a relationship graph.
> The graph is the orchestrator's plan, not a passive transcript of events.**

The Graph-Centric Agent Harness is built for **weak-but-cheap models** — local
deployments, small-context LLMs, models that make mistakes and need structured
retry rather than one-shot accuracy. The design bets that a disciplined harness
can extract reliable results from unreliable models by enforcing:

- **Structured state** (the relationship graph) as the sole working memory
- **Cascade backtracking** on failure — fix downstream, verify upstream
- **Immutable anchor** — user intent is never silently rewritten by the model

### 1.2 Why This Exists

Current agent frameworks (LangGraph, CrewAI, AutoGPT) assume strong models that
plan correctly the first time. They optimize for throughput — decompose once,
dispatch in parallel, merge results. When a sub-task fails, they retry that
single sub-task in isolation.

This works for GPT-4/Claude Opus. It breaks for DeepSeek-v4-flash, local Llama
runs, and any model where the initial plan is likely wrong.

**This harness is designed for the second case.** It treats every failure as a
learning signal that may invalidate upstream assumptions, and it verifies
accordingly.

---

## 2. The Relationship Graph

### 2.1 Three-Layer Architecture (L0 / L1 / L2)

| Layer | Name | Content | Mutability |
|-------|------|---------|------------|
| **L0** | Skeleton | Nodes + edges (structure) | Mutable via patches |
| **L1** | Muscle | Per-node `{responsibility, implementation, design_intent, constraints}` + confidence | Mutable via re-enrichment |
| **L2** | Skin | Raw bytes (source files, configs, schemas) | Never stored in graph; read on demand |

L0 tells you *what depends on what*. L1 tells you *why* and *how*. L2 is ground
truth — the actual files, configs, data that the graph describes.

### 2.2 Node Types

Nodes in the graph fall into two categories:

**Deterministic nodes** — The model knows how to implement them with high
confidence. Example: "Create an HTML page with a `<canvas>` element."

**Exploratory nodes** — The model does NOT know how to implement them and must
discover the approach through search, experimentation, or user clarification.
Example: "Implement snake movement control logic."

Both types coexist on the same graph. The model marks nodes it is uncertain
about, and those become the targets for exploration and repair.

### 2.3 Anchor Node (Root)

The root node **A** represents the user's intent — the task as stated. It is:

- **Immutable.** The model may never rewrite, remove, or replace A.
- **The backtracking terminus.** All cascade verification stops at A.
- **The user's only intervention point.** If backtracking reaches A and A
  itself is found to be ambiguous or infeasible, the harness surfaces the
  issue to the user for clarification.

A is not "absolutely correct" in a mathematical sense. It is *anchored by user
authority* — the model cannot silently reinterpret "refactor the auth module"
as "build a new auth module from scratch" or "the auth module doesn't exist so
I'll refactor the logger instead." If reality contradicts A, the user is
notified.

---

## 3. The Core Loop

### 3.1 Overview

```
User provides: Anchor A + expected outcome D

┌─────────────────────────────────────────────────────────┐
│                                                         │
│  1. PLAN: Main agent reads graph, searches web/         │
│     knowledge, produces path A→B→C→...→D               │
│                                                         │
│  2. EXECUTE: Sub-agents execute nodes step by step      │
│                                                         │
│  3. ON FAILURE at node K:                               │
│     ├── Main agent re-plans K and downstream path       │
│     ├── Cascade-backtrack: verify K's predecessors      │
│     │   along all inbound edges toward A                │
│     └── Preserve intermediate results; verify only      │
│                                                         │
│  4. REPEAT until all nodes from A to D are verified     │
│                                                         │
└─────────────────────────────────────────────────────────┘
```

### 3.2 Phase State Machine

```
                    ┌──────────┐
                    │  GRAPH   │ ← Plan, verify, repair
                    └────┬─────┘
                         │ graph verified
                         ▼
                    ┌──────────┐
                    │   TASK   │ ← Decompose, dispatch sub-agents
                    └────┬─────┘
                         │
              ┌──────────┼──────────┐
              │          │          │
              ▼          ▼          ▼
          all OK    graph error   task error
              │          │          │
              ▼          ▼          ▼
           REVIEW    CASCADE     surfaced to
              │      BACKTRACK   caller
              ▼          │
           DONE     ┌────┴─────┐
                    │ Re-plan  │
                    │ Verify   │
                    │ upstream │
                    └────┬─────┘
                         │
                         ▼
                      GRAPH (re-enter)
```

### 3.3 Main Agent Responsibilities

The main agent (Proposer) is the **sole planner**. It:

1. **Searches** — web, knowledge base, codebase — to discover how similar
   problems have been solved
2. **Plans** — produces nodes and edges on the graph
3. **Distinguishes** known nodes from exploratory nodes
4. **Re-plans** automatically on sub-agent failure — redesigns the failed
   node and its downstream path
5. **Requests user clarification** ONLY when A (the anchor) is ambiguous
   or the expected outcome D is unclear

The main agent does NOT execute tasks directly. It delegates all execution
to sub-agents via the `explore` step (read-only investigation) or the Task
phase dispatch.

### 3.4 Sub-Agent Responsibilities

Sub-agents are **single-shot executors**. Each sub-agent:

1. Receives a node's spec (L0 + L1 + relevant L2 context)
2. Executes tools (bash, web, etc.) to fulfill the spec
3. Reports one of:
   - `success` — node completed, output produced
   - `report_graph_error` — reality contradicts the graph at this node,
     with structured evidence

Sub-agents do **not** edit the graph. They do **not** plan. They do **not**
decide what to do next. They are workers, not thinkers.

---

## 4. Cascade Backtracking

### 4.1 The Principle

> **Nodes in a relationship graph are coupled by default.**
> When a downstream node changes, its predecessors must be verified.

This is the fundamental difference from current agent frameworks. Most
frameworks assume node independence — fixing node C does not affect node B.
We assume the opposite: **B was designed to serve C. If C changes, B may
no longer be fit for purpose.**

### 4.2 The Algorithm

```
On failure at node K:

1. Main agent re-designs K → K' (and potentially K's downstream path)

2. Cascade-backtrack from K':
   For each predecessor P of K' (along all inbound edges):
   
   a. VERIFY: Does P's design still make sense given K''s new requirements?
      - Check P's design logic (L1: responsibility, implementation, design_intent)
      - Check P's output (can P's result feed into K''s expected input?)
   
   b. If P passes both checks → P is preserved as-is, continue backtracking
   
   c. If P fails → mark P for repair, continue backtracking from P
      (P's predecessors may also need verification)
   
   d. If P's design is correct but output is wrong → P is re-executed
      (not re-designed), output is refreshed, continue backtracking
   
   e. Stop when reaching A (A is immutable — surface to user if A is the problem)

3. After backtracking completes:
   - Re-execute any nodes whose outputs were invalidated
   - Resume forward execution from the deepest repaired node
```

### 4.3 Verification Decision

The verification at each backtracking step is performed by the **model**,
not by a fixed rule engine. The model receives:

- The predecessor's L1 (design description) and last output
- The successor's new L1 (updated requirements) and expected input
- The original task context

And decides: *Does this predecessor still work for the new successor?*

This is model-driven because:
- Code tasks may need actual test execution
- Non-code tasks may need semantic comparison
- The verification strategy depends on the domain

### 4.4 Intermediate Results Preservation

When backtracking verifies a node's design as correct, the node's output
is **preserved**. Only nodes that fail verification are re-executed. This
keeps the cost of backtracking proportional to the *scope of change*, not
the *length of the chain*.

### 4.5 DAG Support

Cascade backtracking works on arbitrary DAGs, not just linear chains:

```
        → B1 →
    A →       → D → E
        → B2 → C →
```

If C fails and is redesigned to C':
- Both B1 (via D) and B2 (direct) are predecessors that need verification
- Backtracking follows ALL inbound edges from the changed node
- The verification fan-out is bounded by the graph's in-degree
- Each branch backtracks independently toward A

---

## 5. Exploration and Planning

### 5.1 Two Phases of Planning

**Initial planning (from scratch):**

When the user provides only A and D, with no intermediate nodes:

1. Main agent searches the web / knowledge base for similar problems
2. Main agent identifies nodes it can design with confidence (deterministic)
3. Main agent identifies nodes it is uncertain about (exploratory)
4. Main agent proposes an initial graph with known nodes filled in and
   exploratory nodes marked as placeholders
5. Execution begins; exploratory nodes will trigger sub-agent investigation
   and may produce `report_graph_error` for re-planning

**Re-planning (on failure):**

When sub-agent K reports failure:

1. Main agent receives the failure evidence
2. Main agent searches for alternative approaches to K
3. Main agent produces K' (and potentially modifies K's downstream path)
4. Main agent triggers cascade backtracking from K'

### 5.2 Multi-Path Exploration

When the main agent discovers multiple candidate approaches:

```
Goal A → Search → Three candidate paths found:
  Path 1: A → B  → C  → D
  Path 2: A → X  → Y  → D
  Path 3: A → P  → D
```

The harness supports two strategies, chosen by the model:

- **Depth-first**: Pick the most promising path, execute fully. If it
  fails, backtrack and try the next candidate at the failure point.
- **Breadth-first probe**: Send lightweight exploration sub-agents down
  each candidate path for a fixed small number of steps. Use their
  findings to select the best path before committing to full execution.

### 5.3 Known vs. Unknown Nodes (Example)

Task: "Build a simple Snake game web page"

```
确定节点 (Deterministic — model knows how):
  ├── HTML page structure               [confidence: 0.9]
  ├── Canvas rendering setup            [confidence: 0.85]
  ├── Food random generation            [confidence: 0.9]
  └── Score display                     [confidence: 0.9]

不确定节点 (Exploratory — needs discovery):
  └── Snake movement control logic      [confidence: 0.3]
       ├── Keyboard input handling      [confidence: 0.5]
       ├── Game loop / animation frame  [confidence: 0.4]
       └── Collision detection          [confidence: 0.6]
```

The model builds the graph with deterministic nodes filled in. The
exploratory node "snake movement control" becomes the focus of
sub-agent investigation. If the sub-agent's attempted implementation
fails, the main agent re-plans just that node and its downstream
dependencies, then cascade-backtracks to verify upstream nodes still
produce compatible outputs.

---

## 6. Convergence Guarantees

### 6.1 Hard Budget

- **300 rounds per run.** Each round is one `step()` beat of the state
  machine. A round may be: a Proposer step, a sub-agent dispatch, a
  verification pass, or a cascade-backtrack check.
- When the budget is exhausted, the harness stops and presents:
  - The current graph state
  - Which nodes succeeded and which failed
  - The last failure's evidence
  - A request for user guidance

### 6.2 User Intervention Points

The user is involved in exactly two situations:

1. **Anchor ambiguity (pre-execution).** If A or D is unclear, the main
   agent emits `ask_user` BEFORE drawing any graph nodes.
2. **Budget exhaustion / anchor-level contradiction.** If 300 rounds pass
   without convergence, or if cascade backtracking reaches A and finds
   A itself infeasible, the user is presented with the evidence and
   asked for direction.

Between these two points, the harness runs autonomously — the main agent
re-plans, sub-agents execute, cascade backtracking verifies, all without
user interaction.

### 6.3 Why This Converges

The design converges (or terminates with a clear signal) because:

- **A is fixed.** The backtracking terminus is immutable — no infinite
  regression past the user's intent.
- **Each re-plan learns.** The main agent sees the failure evidence and
  searches for alternatives; it does not retry the same approach.
- **The graph is monotonic in knowledge.** Even failed attempts add
  information — "this approach doesn't work" is recorded in L1 and
  prevents the model from retrying the same dead end.
- **The budget is hard.** 300 rounds is generous enough for complex
  tasks but finite enough to guarantee termination.

---

## 7. Comparison with Current Implementation (v1)

| Aspect | Current v1 | v2 Cascade Backtracking |
|--------|-----------|------------------------|
| **Anchor node** | None — any node can be patched | A is immutable, backtracking terminus |
| **On sub-agent failure** | `GraphInvalid` surfaced to caller; caller decides | Main agent auto-re-plans; cascade-backtrack triggers |
| **Failure scope** | Local repair — fix only the failed node | Cascade — verify all predecessors to A |
| **Node coupling assumption** | Nodes are relatively independent | Nodes are coupled by default |
| **Verification** | Fixed 3-layer (structural + model + L1 sampling) | Model-driven, domain-adaptive |
| **Intermediate results** | Not preserved across re-plan | Preserved, verify-only on backtrack |
| **User involvement** | At every `GraphInvalid` + `Paused` | Only at anchor ambiguity or budget exhaustion |
| **Round budget** | 50 (max_rounds default) | 300 |
| **Model target** | Any model (tiered fast/deep) | Weak-but-cheap models (local, small-context) |
| **Convergence argument** | Insurance fuses (stuck detection, repair budget) | Structural: monotonic knowledge + fixed anchor + hard budget |

---

## 8. Implementation Roadmap

### Phase 1: Graph Foundation Changes

- [ ] Add `Node::immutable` flag to graph types
- [ ] Add `Anchor` node kind or metadata marker
- [ ] Implement `Graph::predecessors_of(node) -> Vec<NodeId>` (inbound edge traversal)
- [ ] Implement `Graph::path_to_anchor(node) -> Vec<NodeId>` (backtracking path)

### Phase 2: Cascade Backtracking Engine

- [ ] New component: `CascadeBacktracker`
  - `backtrack_from(node: NodeId, graph: &Graph) -> CascadeResult`
  - For each predecessor P of the changed node:
    - Call model to verify P's design + output against new successor requirements
    - Return `Preserved | NeedsRepair | NeedsReexecution`
  - Recurse on predecessors that need repair
  - Terminate at anchor node
- [ ] Model-driven verification prompt template

### Phase 3: Auto-Replan on Failure

- [ ] Modify `GraphLoop::step_task_stub()` — on `report_graph_error`:
  - Instead of surfacing `GraphInvalid` to caller
  - Feed failure evidence back to main agent (Proposer)
  - Main agent produces new plan for failed node + downstream
  - Apply new plan as `GraphPatch`
  - Trigger cascade backtracking
- [ ] 300-round budget configuration
- [ ] Budget exhaustion → user-friendly output

### Phase 4: User Intervention Refinement

- [ ] Anchor ambiguity detection in Proposer (already partially implemented
  as Mode B / `ask_user`)
- [ ] Budget exhaustion output format: graph state + failure evidence + guidance request
- [ ] User resume with clarified anchor

### Phase 5: Optimization

- [ ] Intermediate result caching across backtracking rounds
- [ ] Parallel backtracking for DAG nodes with multiple predecessors
- [ ] Breadth-first probe mode for multi-path exploration

---

## 9. Design Principles (Updated)

These extend the v1 principles documented in `ARCHITECTURE.md`:

1. **Model-agnostic.** Never hardcode a model name. All model selection flows
   through `ModelConfig`.

2. **Time-for-space (正确性优先).** Many small precise corrections beat fewer
   batched ones. Each failure is a precision signal.

3. **Cascade repair, never isolated.** When a downstream node changes, its
   upstream dependencies must be verified. Node coupling is the default
   assumption.

4. **Anchor immutability.** The user's intent (node A) is never silently
   rewritten. All backtracking terminates at A. Only the user may change A.

5. **Universality in the model, structure in the graph.** The harness is
   generic across domains. Domain-specific judgment (including verification
   strategy) is delegated to the model.

6. **Scanners are seeds, not the product.** Low-confidence starter graphs
   from scanners; the model is the real graph builder.

7. **Monotonic knowledge.** Even failed attempts add information. The graph
   accumulates knowledge; it never goes backward in understanding.

8. **Weak-model-first.** Design for the model that makes mistakes. A harness
   that works for weak models works even better for strong ones; the reverse
   is not true.

---

## 10. References

- Original design conversation (2026-06-10) — the cascade-backtracking
  mechanism and anchor-node concept
- [ARCHITECTURE.md](../ARCHITECTURE.md) — current v1 implementation design rationale
- [README.md](../README.md) — current project overview
- Graph Harness paper (arXiv:2604.11378) — independent validation of
  graph-centric agent architecture
- Anthropic, "Building Effective Agents" — orchestrator-workers pattern
- SWE-agent (arXiv:2405.15793) — Agent-Computer Interface as first-order
  performance lever
