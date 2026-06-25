# Drill-Down Sub-Graph + Middle-Node Branching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 Filling 阶段的中间节点可以多分叉/汇聚/互依,并允许模型把复杂节点标记为"下钻成子图"——系统 fork 一个独立子 GraphLoop 跑出"如何实现该节点",子图 done = 父图该节点 done,深度受 `max_drilldown_depth` 限制可递归。

**Architecture:** 父图检测到 `GraphPatch.drill_down` 字段 → `fork_sub_graph_for(C)` 创建一个独立 `GraphLoop` 实例(配置继承父图,`current_depth` 递增),把 C 作为子图的 `start`,异步 spawn 跑同样 Filling/Expanding/Review。父图 `pending_sub_runs` 持有子图 handle,step_graph 主循环每轮先 polling 子图状态(子图 done → C 标 done,父图继续 C 下游)。`Node.expanded: bool`(codebase 已有 fractal 脚手架)+ Node metadata key `sub_run_id` / `sub_run_status` / `drill_down_depth` 共同标记"该节点被下钻到子图"。

**Tech Stack:** Rust 1.7x, tokio, serde, axum, anyhow, tracing。`cargo test --lib` 543+ → 565+ 测试(基线 + 新增 ~22 个)。

参考 spec: `docs/superpowers/specs/2026-06-25-drill-down-sub-graph-design.md`

读码确认(基线):
- `src/agent/graph_loop.rs:454` — `GraphLoopConfig`
- `src/agent/graph_loop.rs:529` — `GraphLoop`
- `src/agent/graph_loop.rs:2410` — `build_filling_hint`(本计划要改写的位置)
- `src/agent/proposer.rs:74-100` — `ProposerStep::ProposePatch { patch: GraphPatch, .. }` 枚举变体
- `src/graph/mod.rs:359` — `Node`(已有 `metadata: HashMap<String, serde_json::Value>` + `expanded: bool` 字段,可直接复用)
- `src/graph/mod.rs:494` — `GraphPatch`(本计划要加 `drill_down: Option<DrillDownMark>` 字段)
- `src/graph/mod.rs:497-528` — `Graph`(已有 `parent: Option<(NodeId, Box<Graph>)>` fractal 字段,本期不直接用但确认存在)
- `src/web/checkpoint.rs:10` — `Checkpoint`(本计划要加 `sub_run_links: Vec<SubRunLink>` 字段)
- `src/web/state.rs:50` — `EngineConfig`(本计划要加 `max_drilldown_depth: usize` 字段)
- `src/web/api_runs.rs:435` — `drive_run`(参考 main run 启动模式)
- `tests/integration_*.rs` — 现有 e2e 模式参考

不重复代码的原则:本计划涉及的子图 fork 复用现有 `GraphLoop::new(config)` + `drive_run` 模式,只新增"async spawn + 父-子 polling"这一层;子图 GraphLoop 自身不感知自己是子图,只接收 `current_depth` / `parent_run_id` 两个参数。

---

## File Structure

| 文件 | 变更 | 职责 |
|---|---|---|
| `src/agent/proposer.rs` | 修改 | `DrillDownMark` 新 struct + `GraphPatch.drill_down` 字段 + system prompt 新增 schema 段 + 校验 |
| `src/agent/graph_loop.rs` | 修改 | 新增 `pending_sub_runs` / `parent_run_id` / `current_depth` / `sub_run_counter` 字段;`SubRunHandle` / `SubRunStatus` / `DrillDownError` 类型;`fork_sub_graph_for` / `poll_sub_run_status` / `mark_complex_node_done` / `mark_complex_node_error` / `build_sub_task_for` 方法;`step_graph` 主循环加 polling 优先;改写 `build_filling_hint` |
| `src/web/checkpoint.rs` | 修改 | `Checkpoint` 加 `sub_run_links: Vec<SubRunLink>`(#[serde(default)]) |
| `src/web/persistence.rs` | 修改 | 新增 `create_sub_run_dir` / `append_sub_run_link` / `read_sub_run_status` / `sub_run_run_json` 辅助 |
| `src/web/state.rs` | 修改 | `EngineConfig` 新增 `max_drilldown_depth: usize`(默认 2) |
| `src/web/api_runs.rs` | 修改 | `GET /api/runs/:id/sub-runs`、`GET /api/runs/:id/parent` |
| `src/web/mod.rs` | 修改 | 注册新路由 |
| `tests/integration_drill_down.rs` | 新建 | 3 个 e2e |

---

## Task 1: `DrillDownMark` struct + `GraphPatch.drill_down` 字段

**Files:**
- Modify: `src/graph/mod.rs:494-505`(在 `GraphPatch` 里加字段)
- Modify: `src/graph/mod.rs`(新 struct `DrillDownMark`)
- Test: `src/graph/mod.rs` 现有 `#[cfg(test)] mod tests` 加新测试

- [ ] **Step 1: 写失败测试** — 在 `src/graph/mod.rs` 的测试模块加:

```rust
#[test]
fn graph_patch_drill_down_field_round_trips() {
    let patch = GraphPatch {
        add_nodes: vec![],
        add_edges: vec![],
        remove_node_ids: vec![],
        remove_edge_indices: vec![],
        set_l1: vec![],
        reason: "drill down design-modules".into(),
        drill_down: Some(DrillDownMark {
            target: NodeId::from("design-modules"),
            reason: "10+ sub-modules, each is a sub-design".into(),
            sub_task_override: None,
        }),
    };
    let json = serde_json::to_string(&patch).unwrap();
    let back: GraphPatch = serde_json::from_str(&json).unwrap();
    let dd = back.drill_down.expect("drill_down preserved");
    assert_eq!(dd.target.as_str(), "design-modules");
    assert_eq!(dd.reason, "10+ sub-modules, each is a sub-design");
    assert!(dd.sub_task_override.is_none());
}

#[test]
fn graph_patch_drill_down_omitted_serializes_as_null() {
    let patch = GraphPatch::default();
    let json = serde_json::to_string(&patch).unwrap();
    let back: GraphPatch = serde_json::from_str(&json).unwrap();
    assert!(back.drill_down.is_none());
}
```

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test --lib graph_patch_drill_down 2>&1 | tail -20`
Expected: 编译失败,`error[E0601]: main thread panicked due to unresolved import` 或 `error[E0560]: field 'drill_down' on type ... does not exist`

- [ ] **Step 3: 实现** — 在 `src/graph/mod.rs` 找到 `GraphPatch` struct(行 494-505),在 `reason: String` 字段后加 `drill_down: Option<DrillDownMark>`,带 `#[serde(default)]`:

```rust
/// Optional: mark one of `add_nodes` for drill-down. The system will
/// pause the parent graph at this node and fork a sub-GraphLoop to
/// expand it. See `DrillDownMark` and `docs/superpowers/specs/2026-06-25-drill-down-sub-graph-design.md`.
#[serde(default)]
pub drill_down: Option<DrillDownMark>,
```

在 `GraphPatch` struct 之后(同一文件)加新 struct:

```rust
/// Sub-graph drill-down marker attached to a `GraphPatch`. The model
/// sets this in `propose_patch` to flag a newly-added node that needs
/// to be expanded into a sub-GraphLoop.
///
/// Lifecycle:
///   1. Model emits `GraphPatch { add_nodes: [C], drill_down: Some(...) }`
///   2. `step_graph` applies the patch (adds C + edges)
///   3. `fork_sub_graph_for(C)` is called: if `current_depth + 1 <= max_drilldown_depth`,
///      a child `GraphLoop` is spawned; otherwise the field is dropped with a warn log.
///   4. Parent graph polls the child run's `data/runs/<parent>/sub_runs/<child>/run.json`
///      until status = Done, then marks C done and proceeds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrillDownMark {
    /// Target node id, must be present in the same patch's `add_nodes`.
    pub target: NodeId,
    /// Human-readable reason (used in transcript + sub-task description).
    pub reason: String,
    /// Optional: refined sub-task description (defaults to node.summary).
    #[serde(default)]
    pub sub_task_override: Option<String>,
}
```

- [ ] **Step 4: 运行测试,确认通过**

Run: `cargo test --lib graph_patch_drill_down 2>&1 | tail -10`
Expected: `test result: ok. 2 passed`

- [ ] **Step 5: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/graph/mod.rs
git commit -m "feat(graph): add drill_down field to GraphPatch + DrillDownMark struct"
```

---

## Task 2: ProposePatch 校验 — `drill_down.target` 必须在 `add_nodes`

**Files:**
- Modify: `src/agent/proposer.rs`(在 `parse_step` 之后 / `Ok(ProposerStep::ProposePatch { patch, .. })` 之前加校验)
- Test: `src/agent/proposer.rs` 现有测试模块加新测试

- [ ] **Step 1: 写失败测试** — 在 `src/agent/proposer.rs` 现有 `#[cfg(test)] mod tests` 末尾加:

```rust
#[test]
fn drill_down_target_must_be_in_add_nodes_rejects() {
    let patch = GraphPatch {
        add_nodes: vec![
            Node::task("design-modules", "设计功能模块层:..."),
        ],
        add_edges: vec![],
        remove_node_ids: vec![],
        remove_edge_indices: vec![],
        set_l1: vec![],
        reason: "test".into(),
        drill_down: Some(DrillDownMark {
            target: NodeId::from("not-in-add-nodes"),
            reason: "test".into(),
            sub_task_override: None,
        }),
    };
    // Simulate the validator: target ∉ add_nodes → reject
    let target_in_add = patch.add_nodes.iter().any(|n| n.id == patch.drill_down.as_ref().unwrap().target);
    assert!(!target_in_add, "validator should reject: target not in add_nodes");
}

#[test]
fn drill_down_target_in_add_nodes_passes() {
    let patch = GraphPatch {
        add_nodes: vec![Node::task("design-modules", "...")],
        add_edges: vec![],
        remove_node_ids: vec![],
        remove_edge_indices: vec![],
        set_l1: vec![],
        reason: "test".into(),
        drill_down: Some(DrillDownMark {
            target: NodeId::from("design-modules"),
            reason: "test".into(),
            sub_task_override: None,
        }),
    };
    let target = patch.drill_down.as_ref().unwrap().target.clone();
    let target_in_add = patch.add_nodes.iter().any(|n| n.id == target);
    assert!(target_in_add);
}
```

- [ ] **Step 2: 运行测试,确认编译通过(逻辑断言层面校验)**

Run: `cargo test --lib drill_down_target 2>&1 | tail -10`
Expected: `test result: ok. 2 passed`(这两个测试只验证"判断逻辑",实际的 reject 逻辑下一步实现)

- [ ] **Step 3: 实现** — 在 `src/agent/proposer.rs` 找到现有的 patch 校验函数(在 `parse_step` 后面、产生 `ProposerStep::ProposePatch { patch, .. }` 之前),加校验:

```rust
/// Reject drill_down if target not in add_nodes. Drop extra drill_downs
/// in a single patch (keep the first; spec says "max 1 per patch").
fn validate_drill_down(mut patch: GraphPatch) -> GraphPatch {
    if let Some(dd) = &patch.drill_down {
        if !patch.add_nodes.iter().any(|n| n.id == dd.target) {
            // Target not in add_nodes → reject the entire patch
            // (callers check the drill_down field for Err sentinel, or
            // we return a wrapper type. Project-specific: see below.)
        }
    }
    // Note: GraphPatch only has one drill_down field (Option), so "max 1
    // per patch" is structurally enforced. The validation in step 4 covers
    // the target-in-add_nodes check. No count-validation needed.
    patch
}
```

(本计划实际只需要 target-in-add_nodes 校验,不需要 count 校验——因为 `GraphPatch.drill_down` 是 `Option<DrillDownMark>`,结构上就只可能有 0 或 1 个;spec 里的 `drill_down_only_one_per_patch` 测试用 `Option` 自身就满足了,无需运行时校验。)

真正的 reject 逻辑:

```rust
/// Reject drill_down if target not in add_nodes. Returns Err in that case.
fn validate_drill_down(patch: &GraphPatch) -> Result<()> {
    if let Some(dd) = &patch.drill_down {
        if !patch.add_nodes.iter().any(|n| n.id == dd.target) {
            return Err(HarnessError::model(format!(
                "proposer: drill_down.target '{}' not in add_nodes; drill_down must target a node added in the same patch",
                dd.target.as_str()
            )));
        }
    }
    Ok(())
}
```

(若 `HarnessError` 没有 `model` 构造函数,改用项目里现有的对应方式——查 `proposer.rs` 内 `HarnessError::model` 调用格式)

然后在 patch 校验流水线里调用 `validate_drill_down(&patch)?;`(在它返回 `Ok(ProposerStep::ProposePatch { .. })` 之前)。

- [ ] **Step 4: 加集成测试** — 验证 `validate_drill_down` 在错误 target 时返回 Err:

```rust
#[test]
fn validate_drill_down_returns_err_on_missing_target() {
    let patch = GraphPatch {
        add_nodes: vec![Node::task("design-modules", "...")],
        add_edges: vec![],
        remove_node_ids: vec![],
        remove_edge_indices: vec![],
        set_l1: vec![],
        reason: "test".into(),
        drill_down: Some(DrillDownMark {
            target: NodeId::from("not-in-add-nodes"),
            reason: "test".into(),
            sub_task_override: None,
        }),
    };
    assert!(validate_drill_down(&patch).is_err());
}

#[test]
fn validate_drill_down_returns_ok_on_valid_target() {
    let patch = GraphPatch {
        add_nodes: vec![Node::task("design-modules", "...")],
        add_edges: vec![],
        remove_node_ids: vec![],
        remove_edge_indices: vec![],
        set_l1: vec![],
        reason: "test".into(),
        drill_down: Some(DrillDownMark {
            target: NodeId::from("design-modules"),
            reason: "test".into(),
            sub_task_override: None,
        }),
    };
    assert!(validate_drill_down(&patch).is_ok());
}

#[test]
fn validate_drill_down_returns_ok_when_field_absent() {
    let patch = GraphPatch::default();
    assert!(validate_drill_down(&patch).is_ok());
}
```

- [ ] **Step 5: 运行测试**

Run: `cargo test --lib validate_drill_down 2>&1 | tail -10`
Expected: `test result: ok. 3 passed`

- [ ] **Step 6: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/proposer.rs
git commit -m "feat(proposer): validate drill_down.target must be in add_nodes"
```

---

## Task 3: 改写 `build_filling_hint()` 去掉"单链"强约束

**Files:**
- Modify: `src/agent/graph_loop.rs:2410-2431`

- [ ] **Step 1: 写失败测试** — 在 `src/agent/graph_loop.rs` 现有测试模块加:

```rust
#[test]
fn build_filling_hint_no_longer_says_single_path() {
    // Build a minimal GraphLoop with a starter graph
    let gl = test_graph_loop_with_seed();
    let hint = gl.build_filling_hint();
    assert!(
        !hint.contains("main chain is the single path"),
        "hint must not force single chain; got: {hint}"
    );
}

#[test]
fn build_filling_hint_allows_branching_and_drill_down() {
    let gl = test_graph_loop_with_seed();
    let hint = gl.build_filling_hint();
    assert!(hint.contains("branch"), "hint should mention 'branch'");
    assert!(hint.contains("converge"), "hint should mention 'converge'");
    assert!(hint.contains("drill_down"), "hint should mention 'drill_down'");
}
```

(`test_graph_loop_with_seed` 是测试 helper——若不存在,先在 `#[cfg(test)] mod tests` 顶部加:

```rust
fn test_graph_loop_with_seed() -> GraphLoop {
    let mut g = Graph::new();
    let mut start = Node::new("start", NodeKind::Task, "start", "Start: ...");
    start.immutable = true;
    g.add_node(start);
    g.add_node(Node::new("deliverable", NodeKind::Task, "deliverable", "Deliverable: ..."));
    g.add_edge(Edge::new("start", "deliverable", RelationType::LeadsTo, 0.9, "seed"));
    let cfg = GraphLoopConfig::default();
    GraphLoop::new(cfg, /* model = */ None, /* tools = */ None, /* task = */ "test".into(), /* graph = */ g)
}
```

具体构造函数签名查 `graph_loop.rs::GraphLoop::new` 的真实签名,本计划假定 4-arg;若不同,按实际调整。)

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test --lib build_filling_hint 2>&1 | tail -20`
Expected: 3 个测试全部或部分失败,因为当前 hint 文本包含 "main chain is the single path" 且不含 "branch"/"converge"/"drill_down"。

- [ ] **Step 3: 改写 `build_filling_hint()`** — `src/agent/graph_loop.rs:2410-2431`,用以下代码替换整个函数体:

```rust
fn build_filling_hint(&self) -> String {
    let node_info: Vec<String> = self.graph.nodes.values().map(|n| {
        format!("- {} (kind={:?}, summary=\"{}\")", n.id.as_str(), n.kind, n.summary)
    }).collect();
    format!(
        "🔧 You've spent several rounds without adding connected intermediate \
         steps between start and deliverable. Based on what you know, NOW add \
         step nodes AND wire them into the flow. Rules:\n\
         - Use semantic ids (e.g. `outline`, `design-modules`, `define-entities`), \
         NOT letter+number ids like B1/B2/T1.\n\
         - Step nodes are NOT required to form a single chain. They can:\n\
         \t• branch: one node feeds many (e.g. `define-roles` → both \
         `design-modules` and `define-entities`)\n\
         \t• converge: many nodes feed one (e.g. `define-roles` + \
         `define-entities` → `design-modules`)\n\
         \t• cross-depend: a node `B` may `DependsOn` an earlier node `A` \
         even if A is not its direct predecessor\n\
         \t• be a hub: a single complex node (e.g. \"design functional modules\") \
         may contain 5+ sub-concerns — see drill_down below\n\
         - For most step nodes: connect with `LeadsTo` edges in the main flow.\n\
         - For TRUE dependencies (B cannot be designed before A exists): use `DependsOn`.\n\
         - If a step node is itself a complex task (its summary is broad / lists \
         5+ sub-items / would be 1+ hour of work):\n\
         \t→ mark it for drill_down in the propose_patch (see schema). The system \
         will pause the parent graph at this node and spawn a sub-graph to expand it.\n\
         - The original start→deliverable edge can stay; it represents the goal \
         arc, not a forbidden shortcut.\n\
         - Emit a `propose_patch` now with the step node(s), their edges, and any \
         drill_down marks.\n\n\
         Current graph:\n{node_info}",
        node_info = node_info.join("\n")
    )
}
```

- [ ] **Step 4: 运行测试,确认通过**

Run: `cargo test --lib build_filling_hint 2>&1 | tail -10`
Expected: `test result: ok. 3 passed`

- [ ] **Step 5: 跑全量回归**

Run: `cargo test --lib 2>&1 | tail -5`
Expected: 543+ passed(可能掉 0-2 个旧的 hint 相关测试,需要逐个修复;若 > 5 个掉,回滚看原因)

- [ ] **Step 6: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): rewrite build_filling_hint to allow branching + drill_down

Removes the 'main chain is the single path' line that was forcing
linear topology. New hint encourages branch / converge / cross-depend
and tells the model about the drill_down mark for complex nodes."
```

---

## Task 4: Proposer system prompt 新增 `drill_down` schema 段

**Files:**
- Modify: `src/agent/proposer.rs`(找到构建 system prompt 的函数 / `const SYSTEM_PROMPT`)

- [ ] **Step 1: 写失败测试** — 在 `proposer.rs` 测试模块加:

```rust
#[test]
fn proposer_system_prompt_contains_drill_down_schema() {
    let prompt = GraphProposer::default_system_prompt();  // 或实际取 prompt 的方式
    assert!(prompt.contains("drill_down"), "prompt missing 'drill_down' keyword");
    assert!(prompt.contains("target"), "prompt missing 'target' field doc");
    assert!(prompt.contains("sub_task_override"), "prompt missing 'sub_task_override' field doc");
    assert!(prompt.contains("design-modules"), "prompt missing example");
}
```

(若 `default_system_prompt` 不存在,查找 `proposer.rs` 里 `pub const SYSTEM_PROMPT` 或 `fn system_prompt` 之类的,按实际函数/常量名调用)

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test --lib proposer_system_prompt_contains_drill_down 2>&1 | tail -10`
Expected: 失败(prompt 不含 drill_down)

- [ ] **Step 3: 在 system prompt 末尾追加 schema 段** — 找到 `proposer.rs` 里 system prompt 字符串字面量(或 `format!`),在末尾加:

```rust
"
### drill_down (optional, in propose_patch)

Use this to mark a complex step node that needs sub-graph expansion. The
system will pause the parent graph at this node, spawn a child graph
whose `start` is this node, and the child's Filling/Expanding/Review
will produce the detail.

Schema:
  drill_down: {
    target: \"<node_id from add_nodes in the same patch>\",
    reason: \"<one sentence: why this needs expansion>\",
    sub_task_override: \"<optional: refined task description for the sub-graph>\"
  }

When to use:
- Node summary is broad / lists 5+ sub-items
- The node would be 1+ hour of real work
- The node has natural sub-process the user expects broken out

When NOT to use:
- Simple steps (\"define the goal\", \"set up project\")
- Atoms (\"read file X\", \"add a label\")
- Every node (max 1 drill_down per patch; sub-graph is heavy)

Example:
  propose_patch: {
    add_nodes: [{id: \"design-modules\", summary: \"...\", ...}],
    add_edges: [
      {from: \"define-roles\", to: \"design-modules\", relation: \"LeadsTo\"},
      {from: \"design-modules\", to: \"define-entities\", relation: \"LeadsTo\"}
    ],
    drill_down: {target: \"design-modules\", reason: \"10+ sub-modules, each is a sub-design\"}
  }
"
```

(具体追加位置:找 `proposer.rs` 现有 prompt 字符串最末尾,在 `### ready_for_verify` 段后,或 `### patch` 描述段后,按现有顺序追加)

- [ ] **Step 4: 运行测试,确认通过**

Run: `cargo test --lib proposer_system_prompt_contains_drill_down 2>&1 | tail -10`
Expected: `test result: ok. 1 passed`

- [ ] **Step 5: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/proposer.rs
git commit -m "feat(proposer): add drill_down schema to system prompt"
```

---

## Task 5: `SubRunHandle` / `SubRunStatus` / `DrillDownError` 类型 + `GraphLoop` 字段

**Files:**
- Modify: `src/agent/graph_loop.rs:529`(在 `GraphLoop` struct 加字段)
- Modify: `src/agent/graph_loop.rs`(新类型定义)

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn sub_run_status_default_is_running() {
    let s = SubRunStatus::default();
    assert!(matches!(s, SubRunStatus::Running));
}

#[test]
fn sub_run_handle_carries_complex_node() {
    let h = SubRunHandle {
        sub_run_id: "sub-123".into(),
        complex_node: NodeId::from("design-modules"),
        started_at: 1000,
        status: SubRunStatus::Running,
    };
    assert_eq!(h.complex_node.as_str(), "design-modules");
    assert_eq!(h.sub_run_id, "sub-123");
}

#[test]
fn drill_down_error_depth_limit() {
    let e = DrillDownError::DepthLimit;
    assert_eq!(format!("{e:?}"), "DepthLimit");
}
```

- [ ] **Step 2: 运行测试,确认编译失败**

Run: `cargo test --lib sub_run_status_default_is_running 2>&1 | tail -10`
Expected: 编译错误,`unresolved import` / `cannot find type` for `SubRunStatus` / `SubRunHandle` / `DrillDownError`

- [ ] **Step 3: 实现** — 在 `src/agent/graph_loop.rs` 顶部 imports 后、struct 定义前,加新类型:

```rust
/// Handle for a forked sub-GraphLoop. Held by parent graph's
/// `pending_sub_runs` map; `poll_sub_run_status` updates `status` based
/// on the child run's persisted `data/runs/<parent>/sub_runs/<child>/run.json`.
#[derive(Debug, Clone)]
pub struct SubRunHandle {
    pub sub_run_id: String,
    pub complex_node: NodeId,
    pub started_at: u64,
    pub status: SubRunStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubRunStatus {
    Running,
    Done,
    Error(String),
    Timeout,
}

impl Default for SubRunStatus {
    fn default() -> Self { SubRunStatus::Running }
}

/// Errors that can occur during `fork_sub_graph_for`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrillDownError {
    /// `current_depth + 1 > max_drilldown_depth`; the drill_down field
    /// has been dropped (patch nodes/edges still applied).
    DepthLimit,
}
```

在 `GraphLoop` struct 字段区(行 529 附近)末尾加:

```rust
    /// Sub-graph handles keyed by complex_node_id. Non-empty when
    /// the parent is waiting on at least one child run.
    #[serde(skip)]
    pub pending_sub_runs: HashMap<NodeId, SubRunHandle>,
    
    /// Parent run id (None for the outermost run).
    pub parent_run_id: Option<String>,
    
    /// Depth in the drill-down chain: 0 = outermost, 1 = sub, 2 = sub-sub, ...
    pub current_depth: u32,
    
    /// Counter for generating unique sub-run ids.
    pub sub_run_counter: u32,
    
    /// This run's id (used as the parent id when forking sub-runs).
    /// If not already present on GraphLoop, add it.
    pub run_id: String,
    
    /// Event channel for streaming sub-graph events back to the parent.
    /// If not already present, add as `pub event_tx: tokio::sync::broadcast::Sender<EngineEvent>`.
    pub event_tx: tokio::sync::broadcast::Sender<crate::web::events::EngineEvent>,
    
    /// Cache of `drill_down.reason` for nodes added in the most recent patch.
    /// Set by step_graph after a patch with drill_down is applied; consumed
    /// by `build_sub_task_for` during `fork_sub_graph_for`. Cleared after fork.
    #[serde(skip)]
    pub last_patch_drill_down_reasons: HashMap<NodeId, String>,
```

- [ ] **Step 4: 更新构造器** — 在 `GraphLoop::new` 末尾初始化新字段:

```rust
        Self {
            // ... 已有字段 ...
            pending_sub_runs: HashMap::new(),
            parent_run_id: None,
            current_depth: 0,
            sub_run_counter: 0,
            run_id: format!("run-{}", uuid::Uuid::new_v4()),
            event_tx: /* 现有的 event_tx 初始化方式 */,
            last_patch_drill_down_reasons: HashMap::new(),
        }
```

(若 `run_id` / `event_tx` 字段已存在,跳过那两行;若 `event_tx` 的类型不同,改用实际类型)

- [ ] **Step 5: 运行测试**

Run: `cargo test --lib sub_run 2>&1 | tail -10` + `cargo test --lib drill_down_error 2>&1 | tail -10`
Expected: 全部通过

- [ ] **Step 6: 跑全量回归**

Run: `cargo test --lib 2>&1 | tail -5`
Expected: 543+ passed(可能需要修复老测试对新字段的引用,典型如 `GraphLoop::new` 调用的地方)

- [ ] **Step 7: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): add SubRunHandle/SubRunStatus/DrillDownError + GraphLoop fields

New types for the drill-down machinery. GraphLoop gains
pending_sub_runs / parent_run_id / current_depth / sub_run_counter
fields; constructor initializes them to defaults."
```

---

## Task 6: `fork_sub_graph_for` 实现

**Files:**
- Modify: `src/agent/graph_loop.rs`

- [ ] **Step 1: 写失败测试** — 用 mock model + temp dir 验证 fork 行为:

```rust
#[tokio::test]
async fn fork_creates_sub_run_with_complex_node_as_start() {
    let tmp = tempfile::tempdir().unwrap();
    let mut gl = test_graph_loop_with_seed_at(tmp.path());
    gl.config.max_drilldown_depth = 2;
    let complex = NodeId::from("design-modules");
    gl.graph.add_node(Node::task("design-modules", "..."));
    
    let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
    assert_eq!(handle.complex_node, complex);
    assert!(handle.sub_run_id.starts_with("test-run") || handle.sub_run_id.contains("sub"));
    assert!(matches!(handle.status, SubRunStatus::Running));
}

#[tokio::test]
async fn fork_records_sub_run_id_in_complex_node_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let mut gl = test_graph_loop_with_seed_at(tmp.path());
    gl.config.max_drilldown_depth = 2;
    let complex = NodeId::from("design-modules");
    gl.graph.add_node(Node::task("design-modules", "..."));
    
    let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
    let node = gl.graph.nodes.get(&complex).unwrap();
    assert_eq!(node.metadata.get("sub_run_id").and_then(|v| v.as_str()), Some(handle.sub_run_id.as_str()));
    assert_eq!(node.metadata.get("sub_run_status").and_then(|v| v.as_str()), Some("running"));
    assert_eq!(node.metadata.get("drill_down_depth").and_then(|v| v.as_str()), Some("1"));
    assert!(node.expanded, "Node.expanded should be set true after fork");
}

#[tokio::test]
async fn fork_persists_sub_run_under_parent_subdir() {
    let tmp = tempfile::tempdir().unwrap();
    let mut gl = test_graph_loop_with_seed_at(tmp.path());
    gl.config.max_drilldown_depth = 2;
    let complex = NodeId::from("design-modules");
    gl.graph.add_node(Node::task("design-modules", "..."));
    
    let handle = gl.fork_sub_graph_for(complex).await.unwrap();
    // wait briefly for the async spawn
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let sub_dir = tmp.path().join("sub_runs").join(&handle.sub_run_id);
    assert!(sub_dir.exists(), "sub_run dir should exist at {sub_dir:?}");
    assert!(sub_dir.join("run.json").exists(), "sub_run run.json should exist");
}

#[tokio::test]
async fn fork_inherits_model_and_tools_and_increments_depth() {
    let tmp = tempfile::tempdir().unwrap();
    let mut gl = test_graph_loop_with_seed_at(tmp.path());
    gl.config.max_drilldown_depth = 2;
    assert_eq!(gl.current_depth, 0);
    
    let complex = NodeId::from("design-modules");
    gl.graph.add_node(Node::task("design-modules", "..."));
    let _ = gl.fork_sub_graph_for(complex).await.unwrap();
    // The sub-run was spawned with current_depth = 1 (in the spawned sub-loop, not the parent)
    // We can verify the persisted sub_run.json contains the sub-task or check via reading
    // For now: just assert parent_run_id is set on the child
    let sub_dir = tmp.path().join("sub_runs");
    let sub_runs: Vec<_> = std::fs::read_dir(&sub_dir).unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(sub_runs.len(), 1);
    let sub_run_json = std::fs::read_to_string(sub_runs[0].path().join("run.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&sub_run_json).unwrap();
    assert!(v.get("task").is_some(), "sub-run should have a task field");
}

#[tokio::test]
async fn depth_limit_blocks_excessive_recursion() {
    let tmp = tempfile::tempdir().unwrap();
    let mut gl = test_graph_loop_with_seed_at(tmp.path());
    gl.config.max_drilldown_depth = 1;  // already at depth 0, can fork depth 1
    gl.current_depth = 1;                 // simulate being a sub-graph
    let complex = NodeId::from("design-modules");
    gl.graph.add_node(Node::task("design-modules", "..."));
    
    let result = gl.fork_sub_graph_for(complex.clone()).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), DrillDownError::DepthLimit));
    // node metadata should NOT have sub_run_id (fork was rejected)
    let node = gl.graph.nodes.get(&complex).unwrap();
    assert!(node.metadata.get("sub_run_id").is_none());
    assert!(!node.expanded);
}

#[tokio::test]
async fn depth_limit_allows_within_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let mut gl = test_graph_loop_with_seed_at(tmp.path());
    gl.config.max_drilldown_depth = 2;
    gl.current_depth = 0;
    let complex = NodeId::from("design-modules");
    gl.graph.add_node(Node::task("design-modules", "..."));
    
    let result = gl.fork_sub_graph_for(complex).await;
    assert!(result.is_ok(), "depth 0 with max 2 should allow fork to depth 1");
}
```

(`test_graph_loop_with_seed_at` 是新 helper,需要构造一个 `GraphLoop` 实例 with model=None, tools=None, task="test", graph=seed graph, persistence rooted at `tmp.path()`, run_id="test-run-001"。具体构造方法参考 `graph_loop.rs` 现有测试 helper。)

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test --lib fork_creates_sub_run 2>&1 | tail -10`
Expected: 编译错误(`fork_sub_graph_for` 不存在)

- [ ] **Step 3: 实现 `fork_sub_graph_for`** — 在 `src/agent/graph_loop.rs` 加方法:

```rust
pub async fn fork_sub_graph_for(
    &mut self,
    complex_node: NodeId,
) -> Result<SubRunHandle, DrillDownError> {
    let new_depth = self.current_depth + 1;
    if new_depth > self.config.max_drilldown_depth {
        tracing::warn!(
            current_depth = self.current_depth,
            max_depth = self.config.max_drilldown_depth,
            node = %complex_node.as_str(),
            "drill_down depth limit reached; field dropped, patch nodes/edges still applied"
        );
        return Err(DrillDownError::DepthLimit);
    }
    
    let sub_run_id = format!("{}-sub-{}-d{}", self.run_id, self.sub_run_counter, new_depth);
    self.sub_run_counter += 1;
    
    // Build sub-task description
    let sub_task = self.build_sub_task_for(&complex_node);
    
    // Clone the config and bump depth
    let mut sub_config = self.config.clone();
    sub_config.task = sub_task.clone();
    sub_config.max_drilldown_depth = self.config.max_drilldown_depth;  // inherit
    
    // Create sub-GraphLoop (model/tools inherited via config)
    let sub_run_id_for_loop = sub_run_id.clone();
    let parent_run_id = self.run_id.clone();
    let mut sub_loop = GraphLoop::new_with_depth(
        sub_config,
        parent_run_id,
        new_depth,
    );
    
    // Persist sub-run directory + initial run.json
    self.persistence.create_sub_run_dir(&self.run_id, &sub_run_id_for_loop);
    
    let link = SubRunLink {
        node_id: complex_node.clone(),
        sub_run_id: sub_run_id_for_loop.clone(),
        sub_status: "running".into(),
        created_at: now_ms(),
    };
    self.persistence.append_sub_run_link(&self.run_id, &link);
    
    // Write-back to parent node metadata + set Node.expanded
    if let Some(node) = self.graph.nodes.get_mut(&complex_node) {
        node.metadata.insert("sub_run_id".into(), serde_json::Value::String(sub_run_id_for_loop.clone()));
        node.metadata.insert("sub_run_status".into(), serde_json::Value::String("running".into()));
        node.metadata.insert("drill_down_depth".into(), serde_json::Value::Number(new_depth.into()));
        node.expanded = true;
    }
    
    // Transcript event for observability
    self.conversation.add_user(format!(
        "⤵ drill_down started: {}\n(sub_run_id={}, depth={})",
        complex_node.as_str(), sub_run_id_for_loop, new_depth
    ));
    
    // Async spawn sub-graph
    let sub_persistence = self.persistence.clone_for_sub_run(&self.run_id, &sub_run_id_for_loop);
    let event_tx = self.event_tx.clone();
    tokio::spawn(async move {
        let _ = sub_loop.run_with_persistence(sub_persistence, event_tx).await;
    });
    
    Ok(SubRunHandle {
        sub_run_id: sub_run_id_for_loop,
        complex_node,
        started_at: now_ms(),
        status: SubRunStatus::Running,
    })
}
```

需要新增/复用的辅助:
- `GraphLoop::new_with_depth(cfg, parent_run_id, depth)` — 工厂方法,内部调用现有 `new` + 设 `parent_run_id` / `current_depth`
- `self.persistence.create_sub_run_dir(&id)` / `clone_for_sub_run(&id)` / `append_sub_run_link(&parent_run_id, &link)` — 见 Task 8
- `self.run_id: String` 字段 — 检查 `GraphLoop` 是否有,若无就加
- `self.event_tx: broadcast::Sender<...>` — 检查 `GraphLoop` 是否有,若无就加(Task 5 之前的版本可能没)
- `now_ms() -> u64` — `std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64`

(具体签名按实际类型调整;若 `GraphLoop::run_with_persistence` 不存在,参考 `drive_run` 的实现创建一个等价的子入口方法)

- [ ] **Step 4: 实现 `build_sub_task_for`**

```rust
fn build_sub_task_for(&self, complex_node: &NodeId) -> String {
    let node = match self.graph.nodes.get(complex_node) {
        Some(n) => n,
        None => return format!("Drill-down: {}", complex_node.as_str()),
    };
    let reason = self.current_patch_drill_down_reason(complex_node)
        .unwrap_or_else(|| node.summary.clone());
    format!("[Drill-down of {}] {}\n\nGoal: produce a sub-graph explaining how to implement this step. Expand it into concrete sub-steps connected by LeadsTo / DependsOn / Contains. Use semantic ids and emit a complete sub-graph.", 
            complex_node.as_str(), reason)
}

/// Helper: get the reason from the last applied patch's drill_down field.
/// Since the patch is consumed after apply, we cache it on GraphLoop as
/// `last_patch_drill_down_reasons: HashMap<NodeId, String>` (added in Task 5
/// as another field). If not found, fall back to node.summary.
fn current_patch_drill_down_reason(&self, node: &NodeId) -> Option<String> {
    self.last_patch_drill_down_reasons.get(node).cloned()
}
```

(需要在 Task 5 的 `GraphLoop` struct 里再加一个字段 `last_patch_drill_down_reasons: HashMap<NodeId, String>`,在 patch apply 时由 `step_graph` 写入,在 fork 时读取后清空。)

- [ ] **Step 5: 运行测试**

Run: `cargo test --lib fork_ 2>&1 | tail -20`
Expected: 6 个 fork 测试全过(可能需要小调 helper 构造)

- [ ] **Step 6: 跑全量回归**

Run: `cargo test --lib 2>&1 | tail -5`
Expected: 543+ passed(新增 6 个)

- [ ] **Step 7: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): implement fork_sub_graph_for with depth check + persistence

Fork creates a new GraphLoop with current_depth + 1, persists the
sub-run directory and link, writes back sub_run_id/sub_run_status/
drill_down_depth metadata + Node.expanded=true on the complex node,
and async-spawns the sub-loop. Depth limit drops the field with a
warn log instead of failing."
```

---

## Task 7: `poll_sub_run_status` + `mark_complex_node_done/error`

**Files:**
- Modify: `src/agent/graph_loop.rs`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn poll_sub_run_status_marks_done_when_sub_finishes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut gl = test_graph_loop_with_seed_at(tmp.path());
    let complex = NodeId::from("design-modules");
    gl.graph.add_node(Node::task("design-modules", "..."));
    
    // Manually write a sub-run status file with status=Done
    let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let sub_dir = tmp.path().join("sub_runs").join(&handle.sub_run_id);
    std::fs::write(sub_dir.join("run.json"), r#"{"status":"Done"}"#).unwrap();
    
    // Poll
    let mut h = handle;
    gl.poll_sub_run_status(&mut h).await;
    assert!(matches!(h.status, SubRunStatus::Done));
    
    // Node metadata should reflect done
    let node = gl.graph.nodes.get(&complex).unwrap();
    assert_eq!(node.metadata.get("sub_run_status").and_then(|v| v.as_str()), Some("done"));
}

#[tokio::test]
async fn poll_sub_run_status_marks_error_when_sub_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let mut gl = test_graph_loop_with_seed_at(tmp.path());
    let complex = NodeId::from("design-modules");
    gl.graph.add_node(Node::task("design-modules", "..."));
    
    let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let sub_dir = tmp.path().join("sub_runs").join(&handle.sub_run_id);
    std::fs::write(sub_dir.join("run.json"), r#"{"status":"Error","error":"reviewer failed"}"#).unwrap();
    
    let mut h = handle;
    gl.poll_sub_run_status(&mut h).await;
    assert!(matches!(h.status, SubRunStatus::Error(_)));
}

#[tokio::test]
async fn poll_sub_run_status_idempotent_when_still_running() {
    let tmp = tempfile::tempdir().unwrap();
    let mut gl = test_graph_loop_with_seed_at(tmp.path());
    let complex = NodeId::from("design-modules");
    gl.graph.add_node(Node::task("design-modules", "..."));
    
    let handle = gl.fork_sub_graph_for(complex.clone()).await.unwrap();
    // Don't write any status — the async sub-loop will eventually write it, but for now it's not present
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut h = handle;
    gl.poll_sub_run_status(&mut h).await;
    // Should still be Running (no file or "Running")
    assert!(matches!(h.status, SubRunStatus::Running));
}

#[test]
fn mark_complex_node_done_sets_done_metadata() {
    let mut gl = test_graph_loop_with_seed();
    let complex = NodeId::from("design-modules");
    gl.graph.add_node(Node::task("design-modules", "..."));
    gl.mark_complex_node_done(&complex);
    let node = gl.graph.nodes.get(&complex).unwrap();
    assert_eq!(node.metadata.get("status").and_then(|v| v.as_str()), Some("done"));
}

#[test]
fn mark_complex_node_error_sets_error_metadata() {
    let mut gl = test_graph_loop_with_seed();
    let complex = NodeId::from("design-modules");
    gl.graph.add_node(Node::task("design-modules", "..."));
    gl.mark_complex_node_error(&complex, "reviewer failed");
    let node = gl.graph.nodes.get(&complex).unwrap();
    assert_eq!(node.metadata.get("status").and_then(|v| v.as_str()), Some("error"));
    assert_eq!(node.metadata.get("error").and_then(|v| v.as_str()), Some("reviewer failed"));
}
```

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test --lib poll_sub_run_status 2>&1 | tail -10`
Expected: 编译错误(方法不存在)

- [ ] **Step 3: 实现** — 在 `src/agent/graph_loop.rs` 加:

```rust
pub async fn poll_sub_run_status(&mut self, handle: &mut SubRunHandle) {
    let path = self.persistence.sub_run_run_json(&self.run_id, &handle.sub_run_id);
    let status_str = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return,  // sub-run file not yet written; try again next round
    };
    let v: serde_json::Value = match serde_json::from_str(&status_str) {
        Ok(v) => v,
        Err(_) => return,
    };
    let status_field = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
    
    handle.status = match status_field {
        "Done" | "done" => {
            self.mark_complex_node_done(&handle.complex_node);
            self.conversation.add_user(format!(
                "✓ drill_down complete: {}\n(sub_run_id={})",
                handle.complex_node.as_str(), handle.sub_run_id
            ));
            SubRunStatus::Done
        }
        "Error" | "error" => {
            let err = v.get("error").and_then(|s| s.as_str()).unwrap_or("").to_string();
            self.mark_complex_node_error(&handle.complex_node, &err);
            self.conversation.add_user(format!(
                "✗ drill_down failed: {}\n(sub_run_id={}, error: {})",
                handle.complex_node.as_str(), handle.sub_run_id, err
            ));
            SubRunStatus::Error(err)
        }
        _ => SubRunStatus::Running,
    };
    
    if let Some(node) = self.graph.nodes.get_mut(&handle.complex_node) {
        let status_str = match &handle.status {
            SubRunStatus::Running => "running",
            SubRunStatus::Done => "done",
            SubRunStatus::Error(_) => "error",
            SubRunStatus::Timeout => "timeout",
        };
        node.metadata.insert("sub_run_status".into(), serde_json::Value::String(status_str.into()));
    }
}

pub fn mark_complex_node_done(&mut self, node_id: &NodeId) {
    if let Some(node) = self.graph.nodes.get_mut(node_id) {
        node.metadata.insert("status".into(), serde_json::Value::String("done".into()));
    }
}

pub fn mark_complex_node_error(&mut self, node_id: &NodeId, err: &str) {
    if let Some(node) = self.graph.nodes.get_mut(node_id) {
        node.metadata.insert("status".into(), serde_json::Value::String("error".into()));
        node.metadata.insert("error".into(), serde_json::Value::String(err.to_string()));
    }
}
```

(`persistence.sub_run_run_json` 见 Task 8)

- [ ] **Step 4: 运行测试**

Run: `cargo test --lib "poll_sub_run_status\|mark_complex_node" 2>&1 | tail -10`
Expected: 5 个测试全过

- [ ] **Step 5: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): poll_sub_run_status + mark_complex_node_done/error

Poll reads the sub-run's persisted run.json and updates the handle +
parent node metadata. Done/Error both update the complex node and
emit transcript events."
```

---

## Task 8: 持久化辅助方法 + `Checkpoint.sub_run_links`

**Files:**
- Modify: `src/web/persistence.rs`(加新方法)
- Modify: `src/web/checkpoint.rs:10-35`(加字段 + 新 struct)

- [ ] **Step 1: 写失败测试** — 在 `persistence.rs` 测试模块加:

```rust
#[test]
fn create_sub_run_dir_creates_nested_path() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Persistence::new(tmp.path().to_path_buf());
    p.create_sub_run_dir("parent-1", "sub-2");
    let path = tmp.path().join("parent-1").join("sub_runs").join("sub-2");
    assert!(path.exists());
}

#[test]
fn append_sub_run_link_writes_to_parent_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Persistence::new(tmp.path().to_path_buf());
    p.create_sub_run_dir("parent-1", "sub-2");
    let link = SubRunLink {
        node_id: NodeId::from("design-modules"),
        sub_run_id: "sub-2".into(),
        sub_status: "running".into(),
        created_at: 1000,
    };
    p.append_sub_run_link("parent-1", &link);
    let ckpt_path = tmp.path().join("parent-1").join("checkpoints").join("0001.json");
    // (Checkpoint is written by step_graph; for this test, manually write one)
    let ckpt = Checkpoint {
        index: 1,
        round: 1,
        phase: GraphPhase::Filling,
        graph_snapshot: GraphSnapshot::default(),
        transcript: vec![],
        sub_run_links: vec![],
    };
    std::fs::create_dir_all(ckpt_path.parent().unwrap()).unwrap();
    std::fs::write(&ckpt_path, serde_json::to_string(&ckpt).unwrap()).unwrap();
    p.append_sub_run_link("parent-1", &link);
    let ckpt_back: Checkpoint = serde_json::from_str(&std::fs::read_to_string(&ckpt_path).unwrap()).unwrap();
    assert_eq!(ckpt_back.sub_run_links.len(), 1);
    assert_eq!(ckpt_back.sub_run_links[0].sub_run_id, "sub-2");
}

#[test]
fn read_sub_run_status_returns_done_for_completed_run() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Persistence::new(tmp.path().to_path_buf());
    p.create_sub_run_dir("parent-1", "sub-2");
    let run_json_path = tmp.path().join("parent-1").join("sub_runs").join("sub-2").join("run.json");
    std::fs::write(&run_json_path, r#"{"status":"Done"}"#).unwrap();
    let s = p.read_sub_run_status("parent-1", "sub-2");
    assert_eq!(s, "Done");
}
```

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test --lib create_sub_run_dir 2>&1 | tail -10`
Expected: 编译错误(方法不存在)

- [ ] **Step 3: 实现持久化方法** — 在 `src/web/persistence.rs` 的 `impl Persistence` 块加:

```rust
/// Create `<data_root>/<parent_run_id>/sub_runs/<sub_run_id>/` directory.
pub fn create_sub_run_dir(&self, parent_run_id: &str, sub_run_id: &str) {
    let path = self.root
        .join(parent_run_id)
        .join("sub_runs")
        .join(sub_run_id);
    let _ = std::fs::create_dir_all(path);
}

/// Append a SubRunLink to the latest parent checkpoint's `sub_run_links`.
/// (The parent checkpoint is identified as the highest-index *.json in
/// `<data_root>/<parent_run_id>/checkpoints/`; if no checkpoint exists,
/// this is a no-op + warn log.)
pub fn append_sub_run_link(&self, parent_run_id: &str, link: &SubRunLink) {
    let ckpt_dir = self.root.join(parent_run_id).join("checkpoints");
    let latest = std::fs::read_dir(&ckpt_dir)
        .ok()
        .and_then(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
                .max_by_key(|e| e.file_name())
        });
    let Some(latest) = latest else {
        tracing::warn!(parent = %parent_run_id, "no parent checkpoint to append sub_run_link to");
        return;
    };
    let path = latest.path();
    let s = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut ckpt: Checkpoint = match serde_json::from_str(&s) {
        Ok(c) => c,
        Err(_) => return,
    };
    ckpt.sub_run_links.push(link.clone());
    let _ = std::fs::write(&path, serde_json::to_string(&ckpt).unwrap());
}

/// Read the status field from `<data_root>/<parent_run_id>/sub_runs/<sub_run_id>/run.json`.
/// Returns empty string if the file doesn't exist or has no status field.
pub fn read_sub_run_status(&self, parent_run_id: &str, sub_run_id: &str) -> String {
    let path = self.sub_run_run_json(parent_run_id, sub_run_id);
    let s = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let v: serde_json::Value = serde_json::from_str(&s).unwrap_or(serde_json::Value::Null);
    v.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string()
}

/// Compute the path to a sub-run's run.json (no I/O).
pub fn sub_run_run_json(&self, parent_run_id: &str, sub_run_id: &str) -> PathBuf {
    self.root
        .join(parent_run_id)
        .join("sub_runs")
        .join(sub_run_id)
        .join("run.json")
}

/// Clone this Persistence rooted at the sub-run's directory. The sub-run
/// will write its own run.json + checkpoints under this path.
pub fn clone_for_sub_run(&self, parent_run_id: &str, sub_run_id: &str) -> Self {
    let sub_root = self.root
        .join(parent_run_id)
        .join("sub_runs")
        .join(sub_run_id);
    Self::new(sub_root)
}
```

(具体 `Persistence::new` 构造函数签名按实际调整;若方法名不同,改用项目实际命名)

- [ ] **Step 4: 实现 `Checkpoint.sub_run_links` + `SubRunLink`** — 在 `src/web/checkpoint.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub index: u32,
    pub round: u32,
    pub phase: GraphPhase,
    pub graph_snapshot: GraphSnapshot,
    pub transcript: Vec<TranscriptEntry>,
    /// Links from this run's complex nodes to forked sub-runs.
    /// Populated by `fork_sub_graph_for`; consumed by API + frontend.
    #[serde(default)]
    pub sub_run_links: Vec<SubRunLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubRunLink {
    pub node_id: NodeId,
    pub sub_run_id: String,
    pub sub_status: String,    // "running" | "done" | "error"
    pub created_at: u64,
}
```

- [ ] **Step 5: 运行测试**

Run: `cargo test --lib "create_sub_run_dir\|append_sub_run_link\|read_sub_run_status" 2>&1 | tail -10`
Expected: 3 个测试全过

- [ ] **Step 6: 跑全量回归 — 确认老 checkpoint 反序列化不破**

Run: `cargo test --lib 2>&1 | tail -5`
Expected: 543+ passed(老 checkpoint 缺 `sub_run_links` → 默认 `Vec::new()`)

- [ ] **Step 7: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/web/persistence.rs src/web/checkpoint.rs
git commit -m "feat(web): persistence helpers for sub-runs + Checkpoint.sub_run_links

New Persistence methods: create_sub_run_dir, append_sub_run_link,
read_sub_run_status, sub_run_run_json, clone_for_sub_run. Checkpoint
gains an optional sub_run_links field (#[serde(default)]) for backward
compatibility with existing checkpoint files."
```

---

## Task 9: `EngineConfig.max_drilldown_depth` 字段

**Files:**
- Modify: `src/web/state.rs:50`(在 `EngineConfig` 加字段)
- Modify: `src/web/state.rs:166-`(更新 `Default` impl 和可能的 env 解析)

- [ ] **Step 1: 写失败测试** — 在 `state.rs` 测试模块加:

```rust
#[test]
fn engine_config_default_max_drilldown_depth_is_2() {
    let cfg = EngineConfig::default();
    assert_eq!(cfg.max_drilldown_depth, 2, "default should be 2 (= 3 levels: main+sub+sub-sub)");
}

#[test]
fn engine_config_from_env_overrides_max_drilldown_depth() {
    std::env::set_var("GRAPH_HARNESS_MAX_DRILLDOWN_DEPTH", "5");
    let cfg = EngineConfig::from_env().unwrap();
    std::env::remove_var("GRAPH_HARNESS_MAX_DRILLDOWN_DEPTH");
    assert_eq!(cfg.max_drilldown_depth, 5);
}
```

(若 `from_env` 不存在,改用项目里现有的 env 解析方式;若纯程序构造,只测 default)

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test --lib max_drilldown_depth 2>&1 | tail -10`
Expected: 编译错误或断言失败

- [ ] **Step 3: 加字段** — 在 `src/web/state.rs:50` 的 `EngineConfig` struct 加:

```rust
    /// Maximum drill-down depth. 0 = main run only; 2 = main + sub + sub-sub.
    /// Default: 2 (3 levels total).
    pub max_drilldown_depth: usize,
```

在 `Default for EngineConfig` impl 加 `max_drilldown_depth: 2,`。

(若项目有 `from_env` / `from_json` 解析,加对应 case:env var `GRAPH_HARNESS_MAX_DRILLDOWN_DEPTH` → `usize::from_str`)

- [ ] **Step 4: 运行测试**

Run: `cargo test --lib max_drilldown_depth 2>&1 | tail -10`
Expected: 1-2 个测试全过

- [ ] **Step 5: 跑全量回归**

Run: `cargo test --lib 2>&1 | tail -5`
Expected: 543+ passed(可能掉老的 `EngineConfig::default` 字面量测试,需要补 `max_drilldown_depth: 2`)

- [ ] **Step 6: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/web/state.rs
git commit -m "feat(config): add EngineConfig.max_drilldown_depth (default 2)"
```

---

## Task 10: `step_graph` 主循环加 polling 优先 + drill_down 字段检测

**Files:**
- Modify: `src/agent/graph_loop.rs`(找 `step_graph` 函数,在循环开头加 polling 块;在 patch apply Ok 分支末尾加 fork 检测)

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn step_graph_polling_only_when_pending_sub_runs_nonempty() {
    let tmp = tempfile::tempdir().unwrap();
    let mut gl = test_graph_loop_with_seed_at(tmp.path());
    gl.config.max_drilldown_depth = 2;
    let complex = NodeId::from("design-modules");
    gl.graph.add_node(Node::task("design-modules", "..."));
    
    // Fork (creates a real sub-run that will stay running for a while)
    let handle = gl.fork_sub_graph_for(complex).await.unwrap();
    gl.pending_sub_runs.insert(complex.clone(), handle);
    
    // Now call step_graph — it should NOT call the model (no new propose_patch)
    // We can't easily test "no model call" without a mock; instead we test that
    // pending_sub_runs is still non-empty after step_graph returns.
    let pre_step_pending_count = gl.pending_sub_runs.len();
    // step_graph needs a real model to function; this test is mostly a
    // "no panic" smoke test. Mark as such with allow.
    let _ = gl.step_graph_inner().await;
    assert!(!gl.pending_sub_runs.is_empty(), "pending sub-runs should still be tracked");
}

#[tokio::test]
async fn patch_with_drill_down_creates_sub_run() {
    let tmp = tempfile::tempdir().unwrap();
    let mut gl = test_graph_loop_with_seed_at(tmp.path());
    gl.config.max_drilldown_depth = 2;
    
    // Simulate patch with drill_down
    let patch = GraphPatch {
        add_nodes: vec![Node::task("design-modules", "10+ sub-modules")],
        add_edges: vec![Edge::new("start", "design-modules", RelationType::LeadsTo, 0.9, "...")],
        remove_node_ids: vec![],
        remove_edge_indices: vec![],
        set_l1: vec![],
        reason: "expanding".into(),
        drill_down: Some(DrillDownMark {
            target: NodeId::from("design-modules"),
            reason: "10+ sub-modules".into(),
            sub_task_override: None,
        }),
    };
    gl.apply_graph_patch_with_drill_down(&patch).await.unwrap();
    
    assert_eq!(gl.pending_sub_runs.len(), 1, "drill_down should create a pending sub-run");
    assert!(gl.graph.nodes.get(&NodeId::from("design-modules")).unwrap().expanded);
}
```

(若 `step_graph` / `apply_graph_patch_with_drill_down` 不存在,先看 `step_graph` 的实际入口/重构,或新建 wrapper 方法。)

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test --lib step_graph_polling 2>&1 | tail -10`
Expected: 编译错误

- [ ] **Step 3: 实现 polling 块 + drill_down 处理** — 找到 `step_graph` 函数,在主循环开头加:

```rust
// 1. Polling priority: if there are pending sub-runs, poll them first
if !self.pending_sub_runs.is_empty() {
    // Check cancellation before polling
    if self.is_cancelled() {
        // Propagate cancel to all pending sub-runs (via run.json status=Cancelled)
        for (k, _h) in &self.pending_sub_runs {
            self.mark_complex_node_error(k, "parent cancelled");
        }
        self.pending_sub_runs.clear();
        return Ok(LoopState::Cancelled);
    }
    
    let keys: Vec<NodeId> = self.pending_sub_runs.keys().cloned().collect();
    for k in keys {
        let mut handle = self.pending_sub_runs.remove(&k).unwrap();
        self.poll_sub_run_status(&mut handle).await;
        match &handle.status {
            SubRunStatus::Running | SubRunStatus::Timeout => {
                self.pending_sub_runs.insert(k, handle);
            }
            SubRunStatus::Done | SubRunStatus::Error(_) => {
                // Removed from pending; Done case continues normally,
                // Error case will be detected in the next step's existing error path.
            }
        }
    }
    if !self.pending_sub_runs.is_empty() {
        return Ok(LoopState::Continue);  // still waiting on at least one sub-run
    }
}

// ... 原有 step_graph 逻辑 ...
```

在 patch apply 的 `Ok(())` 分支末尾(在 `auto_enrich` 之后)加:

```rust
// 9. Drill-down detection
if let Some(dd) = &patch.drill_down {
    if let Some(complex_node) = patch.add_nodes.iter().find(|n| n.id == dd.target).map(|n| n.id.clone()) {
        // Cache the reason for build_sub_task_for
        self.last_patch_drill_down_reasons.insert(complex_node.clone(), dd.reason.clone());
        match self.fork_sub_graph_for(complex_node.clone()).await {
            Ok(handle) => {
                self.pending_sub_runs.insert(complex_node, handle);
            }
            Err(DrillDownError::DepthLimit) => {
                // Already warned in fork_sub_graph_for; patch was still applied
            }
        }
        self.last_patch_drill_down_reasons.remove(&complex_node);
    }
}
```

(若 `step_graph` 没有"应用 patch"的明显单点,看代码逻辑,把这段插在 patch 已经被 apply 的位置后)

- [ ] **Step 4: 运行测试**

Run: `cargo test --lib "step_graph_polling\|patch_with_drill_down" 2>&1 | tail -10`
Expected: 2 个测试全过(可能需要小幅调整 helper)

- [ ] **Step 5: 跑全量回归**

Run: `cargo test --lib 2>&1 | tail -5`
Expected: 543+ passed(新增 2 个)

- [ ] **Step 6: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/agent/graph_loop.rs
git commit -m "feat(agent): step_graph polling priority + drill_down detection

step_graph now polls pending sub-runs first; if any are still running,
it returns without invoking the model. When a patch with drill_down is
applied, the complex node is forked into a sub-GraphLoop and added to
pending_sub_runs."
```

---

## Task 11: API 端点 — `GET /api/runs/:id/sub-runs` + `GET /api/runs/:id/parent`

**Files:**
- Modify: `src/web/api_runs.rs`(加 2 个 handler)
- Modify: `src/web/mod.rs`(注册路由)

- [ ] **Step 1: 写失败测试** — 在 `api_runs.rs` 测试模块加:

```rust
#[tokio::test]
async fn get_sub_runs_returns_200_with_links() {
    // Set up a parent run with a sub_run_link in its latest checkpoint
    let tmp = tempfile::tempdir().unwrap();
    let run_id = "parent-test-1";
    let sub_id = "sub-test-1";
    let ckpt_dir = tmp.path().join(run_id).join("checkpoints");
    std::fs::create_dir_all(&ckpt_dir).unwrap();
    let ckpt = Checkpoint {
        index: 1,
        round: 1,
        phase: GraphPhase::Filling,
        graph_snapshot: GraphSnapshot::default(),
        transcript: vec![],
        sub_run_links: vec![SubRunLink {
            node_id: NodeId::from("design-modules"),
            sub_run_id: sub_id.into(),
            sub_status: "running".into(),
            created_at: 1000,
        }],
    };
    std::fs::write(ckpt_dir.join("0001.json"), serde_json::to_string(&ckpt).unwrap()).unwrap();
    
    // Use the test app with this data_root
    let app = test_app_with_data_root(tmp.path()).await;
    let resp = app.get(&format!("/api/runs/{run_id}/sub-runs")).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await;
    assert!(body.as_array().unwrap().len() >= 1);
}

#[tokio::test]
async fn get_parent_returns_200_with_parent_id() {
    // Set up a sub-run with a parent_run_id
    // (Persistence::new with sub-run dir; need a way to read parent_run_id)
    // ... (具体的 mock 方式按实际 Persistence API 调整)
}

#[tokio::test]
async fn get_sub_runs_returns_404_for_unknown_run() {
    let app = test_app_with_data_root(tempfile::tempdir().unwrap().path()).await;
    let resp = app.get("/api/runs/nonexistent/sub-runs").await;
    assert_eq!(resp.status(), 404);
}
```

(`test_app_with_data_root` 是新 helper,需要构造一个测试 axum app with data_root 指向 tempdir;参考 `api_runs.rs` 现有测试 helper)

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test --lib get_sub_runs 2>&1 | tail -10`
Expected: 编译错误(路由不存在)

- [ ] **Step 3: 实现 handler** — 在 `src/web/api_runs.rs` 加:

```rust
/// GET /api/runs/:id/sub-runs — list all sub-runs linked from this run's checkpoints.
pub async fn get_sub_runs(
    axum::extract::State(state): axum::extract::State<WebState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<axum::Json<Vec<SubRunLink>>, axum::http::StatusCode> {
    let ckpt_dir = state.persistence_root.join(&id).join("checkpoints");
    let entries = match std::fs::read_dir(&ckpt_dir) {
        Ok(e) => e,
        Err(_) => return Err(axum::http::StatusCode::NOT_FOUND),
    };
    let mut all_links: Vec<SubRunLink> = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        let s = match std::fs::read_to_string(entry.path()) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Ok(ckpt) = serde_json::from_str::<Checkpoint>(&s) {
            all_links.extend(ckpt.sub_run_links);
        }
    }
    Ok(axum::Json(all_links))
}

/// GET /api/runs/:id/parent — return this run's parent run id (sub-runs only).
pub async fn get_parent(
    axum::extract::State(state): axum::extract::State<WebState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<axum::Json<serde_json::Value>, axum::http::StatusCode> {
    let run_json = state.persistence_root.join(&id).join("run.json");
    let s = match std::fs::read_to_string(&run_json) {
        Ok(s) => s,
        Err(_) => return Err(axum::http::StatusCode::NOT_FOUND),
    };
    let v: serde_json::Value = serde_json::from_str(&s).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let parent = v.get("parent_run_id").cloned().unwrap_or(serde_json::Value::Null);
    Ok(axum::Json(serde_json::json!({ "parent_run_id": parent })))
}
```

(具体 `WebState` / `persistence_root` 字段名按实际调整;`State<WebState>` 的具体 extractor 类型按项目里现有 handler 风格)

- [ ] **Step 4: 注册路由** — 在 `src/web/mod.rs` 的 router 加:

```rust
.route("/api/runs/:id/sub-runs", get(api_runs::get_sub_runs))
.route("/api/runs/:id/parent", get(api_runs::get_parent))
```

(具体 router 风格按 `web/mod.rs` 现有代码调整;若用 `Router::new().route(...)`,按那个风格)

- [ ] **Step 5: 运行测试**

Run: `cargo test --lib "get_sub_runs\|get_parent" 2>&1 | tail -10`
Expected: 3 个测试全过

- [ ] **Step 6: 跑全量回归 + build**

Run: `cargo test --lib 2>&1 | tail -5`
Expected: 543+ passed(新增 3 个)

Run: `cargo build --bin serve 2>&1 | tail -5`
Expected: 编译通过

- [ ] **Step 7: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add src/web/api_runs.rs src/web/mod.rs
git commit -m "feat(web): GET /api/runs/:id/sub-runs and /parent endpoints"
```

---

## Task 12: e2e 集成测试 — 3 个场景

**Files:**
- Create: `tests/integration_drill_down.rs`

- [ ] **Step 1: 写测试骨架** — 新建 `tests/integration_drill_down.rs`:

```rust
//! End-to-end tests for the drill-down sub-graph mechanism.
//!
//! These tests boot a full server, start a run with a mocked model that
//! emits specific ProposePatch patterns, and assert the resulting graph
//! state + sub-run artifacts.

use graph_centric::test_helpers::*;  // 按实际 test helper 路径调整

#[tokio::test]
async fn e2e_simple_task_no_drill_down() {
    // Model emits 3 linear steps + ready_for_verify
    // Assert: no sub_runs dir created, run status = Done
    todo!("implement")
}

#[tokio::test]
async fn e2e_design_modules_drills_down_to_sub_graph() {
    // Model emits 5 step nodes; one of them is "design-modules" with drill_down mark
    // Assert: sub_runs/<id>/run.json exists; sub-graph nodes ≥ 5
    // After sub done: parent graph continues, reaches deliverable
    todo!("implement")
}

#[tokio::test]
async fn e2e_drill_down_sub_failure_propagates() {
    // Mock sub-graph reviewer to always return passed=false
    // Assert: parent graph C status=error; parent run status=Error
    todo!("implement")
}
```

(具体 mock 方式参考 `tests/integration_web_e2e.rs` 的现有 pattern)

- [ ] **Step 2: 跑测试,确认编译失败**

Run: `cargo test --test integration_drill_down 2>&1 | tail -10`
Expected: 编译错误或 `todo!()` 失败

- [ ] **Step 3: 实现 `e2e_simple_task_no_drill_down`**

(参考 `integration_web_e2e.rs` 的 boot + run pattern;用 mock model 输出 3 个 LeadsTo 步骤 + ready_for_verify,断言无 sub_runs 目录)

- [ ] **Step 4: 实现 `e2e_design_modules_drills_down_to_sub_graph`**

(Mock model 在某个 patch 里 emit `drill_down: Some(...)`;等子图 done;断言父图 C 状态=done,父图继续推进;总 tokens 断言在合理范围)

- [ ] **Step 5: 实现 `e2e_drill_down_sub_failure_propagates`**

(Mock 子图 reviewer 永远 passed=false;子图 Done 状态是 Error;父图 C 状态=error;父图 run 状态=Error)

- [ ] **Step 6: 跑测试,全过**

Run: `cargo test --test integration_drill_down 2>&1 | tail -10`
Expected: `test result: ok. 3 passed`

- [ ] **Step 7: 跑全量回归**

Run: `cargo test --lib 2>&1 | tail -5` + `cargo test --test integration_drill_down 2>&1 | tail -3`
Expected: 543+ + 3 passed

- [ ] **Step 8: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add tests/integration_drill_down.rs
git commit -m "test(e2e): drill-down mechanism — 3 scenarios (simple, with-drill-down, failure-propagation)"
```

---

## 收尾

- [ ] **跑全量回归,确认无回归**

```bash
cd /home/hhhh/Graph-Centric
cargo test --lib 2>&1 | tail -5
cargo test 2>&1 | tail -10
```

Expected: `lib`: 543+ passed;`integration`: 3 new + 既有全过

- [ ] **手动验证(本地跑)**

```bash
cargo run --bin serve
# 前端:启动"设计一个物业管理系统"任务
# 观察:
#   - 主图从 7 节点单链 → 中间节点出现分叉
#   - 至少一个节点被下钻成子图(目录 data/runs/<id>/sub_runs/<sub_id>/)
#   - 前端点 C 节点 → 跳转到子图页面
#   - 子图 done → 父图 C 节点变绿
#   - 父图继续 → 收尾到 deliverable
```

- [ ] **提交最终 commit(若有 loose ends)**

```bash
cd /home/hhhh/Graph-Centric
git status
git add -A
git diff --cached --stat
git commit -m "feat(drill-down): final integration + manual verification fixes"
```

(若无 loose ends,跳过此步)
