# 复杂节点下钻成子图 + 中间节点允许多分叉 设计

日期:2026-06-25
状态:设计已确认(逐节),待用户审阅 spec
系列:接 [[2026-06-21-graph-direction-leadsto-design]] 之后的"图结构丰富化"第二步(第一步是孤儿检查 + LeadsTo 关系图谱)。

## Context

实测(run `37fe825c`,task="设计一个物业管理系统"):模型最终产出 8 节点 7 边,**笔直的单链**:

```
start → identify-layers → define-roles → design-modules → define-entities
      → tech-architecture → external-interfaces → deliverable
```

每个中间节点 `in=1, out=1`,完全是线性结构。但物业管理系统本质上是一个多层、有复杂依赖的系统:
- "数据实体层"应该被多个模块引用(住户、车辆、费用、工单等)
- "外部接口层"(支付、IoT、地图)应该被多个功能模块依赖
- "技术架构层"是底层支撑,被所有上层模块依赖
- "设计功能模块层"(`design-modules`)本身是 10+ 个子模块的合集,完全应该被下钻展开

**根因(读码坐实)**:`graph_loop.rs::build_filling_hint()`(line 2417-2425)显式告诉模型:

> "Every step node MUST sit on the path: connect with LeadsTo edges so it reads start → step → … → deliverable."
> "When steps are wired between start and deliverable, delete the original direct start→deliverable edge via `remove_edge_indices` **so the main chain is the single path**."

**`build_filling_hint` 在字面层级强制了单链拓扑**。这与 `2026-06-21-graph-direction-leadsto-design` 的设计原意(中间节点关系由模型自由决定)冲突——设计文档说"系统不预设中间结构",但 prompt 实际预设了。

**已存在但不适用的机制**:`cascade_expand.rs` 是 subagent **执行层**的下钻(把 T1 展开成 T1-A/T1-B/T1-C 用于并行执行),不适用于 **L0 图构建层**。`local_subgraph()` 是局部子图提取(给 repairer 用),也不是构建机制。

## 已确认决策(方案 A:独立子 GraphLoop)

> **一句话不变量**:`start → deliverable` 主轴仍用 `LeadsTo`,主轴只接入"被下钻的复杂节点 C"作为入口/出口(进出各 1 条边);**C 内部不再强制单链**——可以多分叉、汇聚、互依;**C 本身可被标记下钻成子图**——子图用一套独立的 Filling/Expanding/Review 跑出"如何实现 C"的子步骤,子图 done = C done。

1. **中间节点不再强制单链**:`build_filling_hint` 删掉"main chain is the single path",改为鼓励 branch / converge / cross-depend。
2. **模型主动标记下钻**:`ProposerStep::ProposePatch` 新增可选 `drill_down: { target, reason, sub_task_override? }` 字段,目标节点必须在同 patch 的 `add_nodes` 里。
3. **独立子 GraphLoop 跑下钻**:父图 fork 一个全新 GraphLoop,把 C 作为子图的 `start`,走完整 Filling/Expanding/Review,**复用现有代码不变**。子图没有强制的 deliverable 节点,子图本身 ready_for_verify = done。
4. **父图挂起/恢复**:有 pending 子图 → 父图 step_graph 不发新 propose_patch,只 polling 子图状态;子图 done → C 标 done → 父图继续 C 下游。
5. **子图可以递归下钻**:子图与父图享有**同一套 `drill_down` 机制**,可继续 fork 子子图、孙图。深度受 `EngineConfig::max_drilldown_depth` 限制(默认 2,即允许 main → sub → sub-sub 三层)。
6. **节点 metadata 复用**:Node 不加新 struct 字段,用现有 `metadata: HashMap<String, String>` 存 `sub_run_id` / `sub_run_status` / `drill_down_depth`(均由系统在 fork 时回填;模型标记下钻走 `ProposePatch.drill_down` 字段而非 metadata key)。
7. **嵌套持久化**:子 run 存 `data/runs/<parent>/sub_runs/<child>/`(递归嵌套,3 层时是 `data/runs/<main>/sub_runs/<sub1>/sub_runs/<sub2>/`),父图 Checkpoint 新增 `sub_run_links: Vec<SubRunLink>`,删父 run 自动级联清子。
8. **错误复用**:`sub_run_status=error` → 父图 C 失败 → 走现有 GraphInvalid 路径(reviewer judge 评 C 失败),不引入新机制。
9. **资源保护**:以**层**为单位,不是以**个数**为单位:`EngineConfig::max_drilldown_depth: usize`(默认 2,即最多 main + sub + sub-sub = 3 层);单 patch 最多 1 个 drill_down(超出 → 该字段被丢弃 + warn log)。

## 架构 / 组件落点

### 1. 父图侧状态变更(`src/agent/graph_loop.rs`)

#### 1.1 `GraphLoop` 新增字段
```rust
pub struct GraphLoop {
    // ... 已有字段 ...
    
    /// 下钻生成的子图 handle 集合(complex_node_id → 子 run 句柄)
    #[serde(skip)]
    pending_sub_runs: HashMap<NodeId, SubRunHandle>,
    
    /// 父 run_id(子图 fork 时需要)
    parent_run_id: Option<String>,
    
    /// 当前 run 的下钻深度(0 = 最外层 main run, 1 = sub, 2 = sub-sub, ...)
    current_depth: u32,
    
    /// 子图 run_id 计数器(防 ID 冲突)
    sub_run_counter: u32,
}

pub struct SubRunHandle {
    pub sub_run_id: String,
    pub complex_node: NodeId,
    pub started_at: u64,    // unix ms
    pub status: SubRunStatus,
}

pub enum SubRunStatus {
    Running,
    Done,
    Error(String),
    Timeout,
}
```

#### 1.2 `step_graph` 主循环改造
每轮 step_graph 顺序:
1. **检查 `pending_sub_runs`**:非空 → polling 每个 handle
   - running/timeout-pending → 不做事,等下轮
   - done → C 标 done,`pending_sub_runs.remove(&C)`,transcript 写 `drill_down_complete`
   - error → 父图 C 标 error,触发 GraphInvalid,return
2. **空** → 正常 Filling/Expanding 逻辑
3. **patch apply 时**(现有 Ok 分支末尾)检测 `patch.drill_down`:
   - 存在且 target 合法 → 调 `fork_sub_graph_for(C)` → 把 handle 写入 `pending_sub_runs`
   - 父图本轮不再发新 propose_patch(polling only)

#### 1.3 新方法:`fork_sub_graph_for(complex_node: NodeId) -> SubRunHandle`
```rust
fn fork_sub_graph_for(&mut self, complex_node: NodeId) -> Result<SubRunHandle, DrillDownError> {
    let new_depth = self.current_depth + 1;
    if new_depth > self.config.max_drilldown_depth {
        // 超过深度限制 → 静默丢弃 drill_down 字段(warn log),patch 的节点/边照常 apply
        tracing::warn!(
            current_depth = self.current_depth,
            max_depth = self.config.max_drilldown_depth,
            node = %complex_node.as_str(),
            "drill_down depth limit reached; field dropped, patch nodes/edges still applied"
        );
        return Err(DrillDownError::DepthLimit);
    }
    
    let sub_run_id = format!("{}-sub-{}-{}", self.run_id, self.sub_run_counter, new_depth);
    self.sub_run_counter += 1;
    
    // 子任务描述:优先用 drill_down.sub_task_override,否则 reason + summary
    let sub_task = self.build_sub_task_for(&complex_node);
    
    // 子图 config:继承父图所有配置,current_depth 递增
    // (子图同样拥有 drill_down 能力,可继续递归 fork)
    let sub_config = self.config.clone()
        .with_task(sub_task)
        .with_current_depth(new_depth);
    
    let sub_loop = GraphLoop::new(sub_config)
        .with_parent_run_id(self.run_id.clone());
    
    // 持久化父-子链接
    self.persistence.create_sub_run_dir(&sub_run_id);
    let link = SubRunLink {
        node_id: complex_node.clone(),
        sub_run_id: sub_run_id.clone(),
        sub_status: "running".into(),
        depth: new_depth,
        created_at: now_ms(),
    };
    self.persistence.append_sub_run_link(&link);
    
    // 回填到父图节点 metadata
    if let Some(node) = self.graph.nodes.get_mut(&complex_node) {
        node.metadata.insert("sub_run_id".into(), sub_run_id.clone());
        node.metadata.insert("sub_run_status".into(), "running".into());
        node.metadata.insert("drill_down_depth".into(), new_depth.to_string());
    }
    
    // 异步 spawn 子图
    let persistence = self.persistence.clone_for_sub_run(&sub_run_id);
    let event_tx = self.event_tx.clone();
    tokio::spawn(async move {
        sub_loop.run_with_persistence(persistence, event_tx).await;
    });
    
    Ok(SubRunHandle { sub_run_id, complex_node, started_at: now_ms(), status: SubRunStatus::Running })
}

enum DrillDownError {
    DepthLimit,    // 超过 max_drilldown_depth,该字段被丢弃
}
```

#### 1.4 新方法:`poll_sub_run_status(handle: &mut SubRunHandle)`
```rust
async fn poll_sub_run_status(&mut self, handle: &mut SubRunHandle) {
    let path = self.persistence.sub_run_run_json(&handle.sub_run_id);
    let status = match read_status(&path) {
        Ok(s) => s,
        Err(_) => return,    // 读不到就下轮再试
    };
    handle.status = match status.as_str() {
        "Done" | "done" => {
            self.mark_complex_node_done(&handle.complex_node);
            self.conversation.add_user(format!(
                "✓ drill_down complete: {}\n(sub_run_id={}, completed)",
                handle.complex_node.as_str(), handle.sub_run_id
            ));
            SubRunStatus::Done
        }
        "Error" | "error" => {
            let err = read_error(&path).unwrap_or_default();
            self.mark_complex_node_error(&handle.complex_node, &err);
            self.conversation.add_user(format!(
                "✗ drill_down failed: {}\n(sub_run_id={}, error: {})",
                handle.complex_node.as_str(), handle.sub_run_id, err
            ));
            SubRunStatus::Error(err)
        }
        _ => SubRunStatus::Running,
    };
    // 同步 metadata
    if let Some(node) = self.graph.nodes.get_mut(&handle.complex_node) {
        node.metadata.insert("sub_run_status".into(), status_to_str(&handle.status).into());
    }
}
```

#### 1.5 `build_filling_hint()` 改写
**当前** (line 2410-2431) 强制单链,**改为**:
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

### 2. Schema 变更(`src/agent/proposer.rs`)

```rust
// 现有 ProposePatch 加一个可选字段
pub struct ProposePatch {
    pub add_nodes: Vec<NodePatch>,
    pub add_edges: Vec<EdgePatch>,
    pub remove_edge_indices: Vec<usize>,
    pub update_l1: Vec<L1Update>,
    pub ready_for_verify: Option<String>,
    /// NEW: 标记某个新节点需要下钻成子图
    pub drill_down: Option<DrillDownMark>,
}

pub struct DrillDownMark {
    /// 目标节点 id,必须在 add_nodes 里
    pub target: NodeId,
    /// 一句话:为什么需要下钻(可读,用于 transcript + 子任务描述)
    pub reason: String,
    /// 可选:更具体的子任务描述(覆盖默认 = node.summary)
    pub sub_task_override: Option<String>,
}
```

**校验规则**(`patch` apply 前):
- `drill_down.target` 不在 `add_nodes` → 整个 patch 拒绝(同 add_nodes 不存在)
- 单 patch `drill_down` 数量 > 1 → 仅保留第一个,其余 warn + drop
- `fork_sub_graph_for` 时若 `new_depth > max_drilldown_depth` → 该字段 drop + warn log(patch 节点/边照常 apply,只是不下钻)

### 3. Proposer system prompt 新增(给模型讲 `drill_down`)

在 `proposer.rs` 的 system prompt 加 schema 段:
```
### drill_down (optional, in propose_patch)

Use this to mark a complex step node that needs sub-graph expansion. The
system will pause the parent graph at this node, spawn a child graph whose
`start` is this node, and the child's Filling/Expanding/Review will produce
the detail.

Schema:
  drill_down: {
    target: "<node_id from add_nodes in the same patch>",
    reason: "<one sentence: why this needs expansion>",
    sub_task_override: "<optional: refined task description for the sub-graph>"
  }

When to use:
- Node summary is broad / lists 5+ sub-items
- The node would be 1+ hour of real work (writing a full design, building a subsystem)
- The node has natural sub-process that the user would expect to see broken out

When NOT to use:
- Simple steps ("define the goal", "set up project")
- Atoms ("read file X", "add a label")
- Every node (max 1 drill_down per patch; sub-graph is heavy)

Example:
  propose_patch: {
    add_nodes: [{id: "design-modules", summary: "...", ...}],
    add_edges: [
      {from: "define-roles", to: "design-modules", relation: "LeadsTo"},
      {from: "design-modules", to: "define-entities", relation: "LeadsTo"}
    ],
    drill_down: {target: "design-modules", reason: "10+ sub-modules, each is a sub-design"}
  }
```

### 4. 数据模型 / 持久化(`src/graph/mod.rs` + `src/web/persistence.rs`)

#### 4.1 Node metadata keys
| Key | Value | 含义 | 谁写 |
|---|---|---|---|
| `sub_run_id` | `"uuid-v4"` | 子图创建后回填 | 系统(fork 时) |
| `sub_run_status` | `"running"` / `"done"` / `"error"` | 子图当前状态 | 系统(polling 时更新) |
| `drill_down_depth` | `"1"` / `"2"` | 节点被下钻到的层数 | 系统(fork 时) |

**Node 结构体本身不增字段**,复用 `metadata: HashMap<String, String>`。序列化反序列化向后兼容。

**注**:模型标记下钻的载体是 `ProposePatch.drill_down: { target, reason, sub_task_override }` 字段(在 `add_nodes` 同 patch 里),不是 metadata key。`requires_drilldown` / `drill_down_reason` 之类 metadata key 跟 patch 字段重复,本设计**不引入**。

#### 4.2 目录布局(嵌套)
```
data/runs/<parent_run_id>/
├── run.json              # 父 run 状态
├── checkpoints/          # 父图快照(每个新增 sub_run_links)
│   └── 0001.json
└── sub_runs/
    ├── <sub_run_id_1>/
    │   ├── run.json
    │   ├── checkpoints/
    │   └── transcript.jsonl
    └── <sub_run_id_2>/
        └── ...
```

#### 4.3 Checkpoint 新增字段(`src/web/checkpoint.rs`)
```rust
pub struct Checkpoint {
    pub index: u32,
    pub round: u32,
    pub phase: GraphPhase,
    pub graph_snapshot: GraphSnapshot,
    pub transcript: Vec<TranscriptEntry>,
    /// 父 run → 子 run 链接(只有父 run 的 checkpoint 才有值)
    #[serde(default)]
    pub sub_run_links: Vec<SubRunLink>,
}

pub struct SubRunLink {
    pub node_id: NodeId,
    pub sub_run_id: String,
    pub sub_status: String,    // "running" / "done" / "error"
    pub created_at: u64,       // unix ms
}
```

老 checkpoint 缺 `sub_run_links` → 默认 `Vec::new()`,行为不变。

#### 4.4 新增 API 端点
| Method | Path | 用途 |
|---|---|---|
| `GET` | `/api/runs/:id/sub-runs` | 列出该 run 的所有子 run |
| `GET` | `/api/runs/:id/parent` | 查该 run 的父 run(子查父,只读) |

前端 C 节点点击 → `GET /api/runs/<parent>/sub-runs` → 拿到 `sub_run_id` → 跳转 `/runs/<sub_run_id>`(复用现有路由)。

#### 4.5 `EngineConfig` 新增
```rust
pub struct EngineConfig {
    // ... 已有字段 ...
    /// 最大下钻深度(0 = main run, 1 = sub, 2 = sub-sub, ...)
    /// 默认 2 = 允许 main → sub → sub-sub 共 3 层
    pub max_drilldown_depth: usize,
}
```

### 5. 错误处理 / 边界

| 场景 | 处理 |
|---|---|
| 子图正常 Done | 父图 C 标 done,`pending_sub_runs` 移除,transcript 写 `drill_down_complete` |
| 子图 Error | 父图 C 标 error,`pending_sub_runs` 移除,**走现有 GraphInvalid 路径** |
| 子图超时(> max_rounds 触发) | 父图把 C 标 timeout,走 GraphInvalid |
| 子图死循环(子图内 model 又发 drill_down) | 子图拥有完整 drill_down 能力可继续递归;深度由 `max_drilldown_depth` 限制,达到上限 → drill_down 字段 drop + warn log(节点/边照常 apply) |
| 模型滥用 drill_down(每节点都标) | 单 patch 限制 1 个;**下钻深度**限制 `max_drilldown_depth`,超出 → drop 字段 + warn log(patch 节点/边照常 apply) |
| 父图在等待子图时用户 cancel | cancel token 向下传播:父 cancel → 父 polling 检测到 → 子 cancel_token 触发,两级 cancel |
| 子图内 orphan(start=C 之后到不了任何节点) | 复用现有 `replay_from_anchor` + `run_verify_and_maybe_repair`,**子图走完整收敛逻辑** |
| 磁盘 / IO 错误 | step_graph 捕获 → log → 子图 link 标 error,父图走 GraphInvalid |
| 父图在 fork 子图前 run 被 cancel | 不 fork,正常 cancel |

### 6. Transcript 事件(用户可观测)

```
[user] ⤵ drill_down started: design-modules
       (sub_run_id=abc-123, reason="10+ sub-modules, each is a sub-design")

[user] ✓ drill_down complete: design-modules
       (sub_run_id=abc-123, sub_rounds=4, sub_tokens=8500)

[user] ✗ drill_down failed: design-modules
       (sub_run_id=abc-123, error="reviewer failed: ...")
```

## 数据流

```
父图 Filling 状态
  Step N:
    model emit ProposePatch {
      add_nodes: [C],
      add_edges: [...],
      drill_down: { target: C, reason: "..." }
    }
  → apply patch(C 入图,边入图)
  → 检测 drill_down 字段
  → fork_sub_graph_for(C):
      - new_depth = current_depth + 1,若 > max_drilldown_depth → 丢弃该字段 + warn log(patch 节点/边仍 apply)
      - 建子 GraphLoop(config 继承,current_depth = new_depth)
      - 异步 spawn(子图同样拥有完整 drill_down 能力)
      - 回填 C.metadata["sub_run_id"], "sub_run_status"="running", "drill_down_depth"=new_depth
      - 父图 transcript 写 "drill_down started (depth=N)"
  → pending_sub_runs.insert(C, handle)
  → 父图 step_graph 下轮进入 polling-only 状态

父图 polling:
  每轮:
    for handle in pending_sub_runs:
      poll_sub_run_status(handle)
      if Done → mark C done, transcript "drill_down complete"
      if Error → mark C error, 走 GraphInvalid
    若仍有 running → return(不发新 patch)
    若全 done → 清空 pending,继续 Filling/Expanding

子 GraphLoop(独立 task):
  start = C(强加,只读)
  Filling → Expanding → Review
  走完 → status = Done,父图 polling 收到
```

## 测试

### 单元测试(`cargo test --lib`)

**fork + 状态机**(`src/agent/graph_loop.rs`):
- `fork_creates_sub_run_with_complex_node_as_start`:父图 C → fork → 子图 start == C
- `fork_records_sub_run_id_in_complex_node_metadata`:C.metadata 包含 `sub_run_id` 字段
- `fork_persists_sub_run_under_parent_subdir`:`data/runs/<parent>/sub_runs/<id>/run.json` 存在
- `fork_inherits_model_and_tools_and_increments_depth`:子图 config == 父图 config,且 `current_depth` = 父 + 1
- `sub_graph_can_drill_down_to_grandchild`:子图内 model 标记节点 → 子子图被创建(depth + 1)
- `depth_limit_blocks_excessive_recursion`:depth 2 的子图发 drill_down → 字段 drop + warn log,patch 节点/边照常 apply
- `parent_suspends_when_pending_sub_runs_nonempty`:有 pending → step_graph 不发新 propose_patch
- `parent_resumes_after_sub_done`:子图 status=done → C 标 done → pending_sub_runs 移除
- `parent_propagates_sub_error`:子图 status=error → 父图 GraphInvalid
- `parent_polling_is_idempotent`:子图 status=running → 父图不做事、不 fork 重复
- `multiple_pending_sub_runs_handled`:2 个 C pending → 父图 polling 都正确(不并行,但能容忍)
- `parent_cancel_propagates_to_sub_runs`:父 cancel → 子 cancel_token 触发

**Schema 校验**(`src/agent/proposer.rs`):
- `drill_down_target_must_be_in_add_nodes`:target 不在 → patch 拒绝
- `drill_down_only_one_per_patch`:2 个 drill_down → 保留第一个,其余 drop+warn
- `drill_down_depth_limit_blocks_excessive`:depth 2 的 run 发 drill_down(patch 仍可 apply 节点/边)→ drill_down 字段 drop + warn log
- `drill_down_depth_allows_within_limit`:depth 0/1 的 run 发 drill_down → 子图正常 fork

**Prompt 改动**(`build_filling_hint`):
- `hint_no_longer_says_single_path`:新文本**不包含** "main chain is the single path"
- `hint_allows_branching`:新文本**包含** "branch"、"converge" 关键词
- `hint_mentions_drill_down`:新文本**包含** "drill_down" 关键词

**Node metadata 兼容性**(`src/graph/mod.rs` + `src/web/checkpoint.rs`):
- `metadata_drilldown_keys_round_trip`:序列化 → 反序列化 → key 完整保留
- `old_checkpoints_without_sub_run_links_deserialize`:老 checkpoint 缺字段 → 默认空

**API 端点**(`src/web/api_runs.rs`):
- `get_sub_runs_returns_links`:父 run 的 `/sub-runs` 返回所有 sub_run_links
- `get_parent_returns_parent_id`:子 run 的 `/parent` 返回父 run_id
- `unknown_run_returns_404`:不存在的 run id → 404

### 集成测试(`tests/integration/drill_down_e2e.rs`)

- `e2e_property_management_drills_into_design_modules`:
  1. 启动 run,task="设计一个物业管理系统"
  2. 等 model 标记 design-modules 下钻
  3. 断言:父 transcript 含 `drill_down started: design-modules`
  4. 断言:`data/runs/<parent>/sub_runs/<id>/run.json` 存在
  5. 等子图 done(子图 nodes ≥ 5,子图 ready_for_verify)
  6. 断言:父图 C 标 done,父图继续推进到 deliverable
  7. 断言:总 token 消耗符合预期(子图 + 父图)
- `e2e_simple_task_no_drill_down`:
  1. task="列出 Rust 的 5 个核心特性"
  2. 断言:父图无 drill_down 事件,直接走 start→...→deliverable 单层
- `e2e_drill_down_sub_failure_propagates`:
  1. mock 子图 reviewer 永远返回 `passed=false`
  2. 断言:父图 C 状态=error,父图 GraphInvalid

### 回归

- 全量 `cargo test --lib` 543+ 通过(新测试加进来后)
- 老 `data/runs/*/checkpoints/*.json` 反序列化无报错
- `build_filling_hint` 触发条件(`filling_rounds_without_nodes == 3`)不动

### 手动验证(本地跑)

```bash
cargo run --bin serve
# 前端:启动"设计一个物业管理系统"任务
# 观察:
#   - 主图从 7 节点单链 → 中间节点出现分叉
#   - 至少一个节点被下钻成子图
#   - 前端点 C 节点 → 跳转到子图页面
#   - 子图 done → 父图 C 节点变绿
#   - 父图继续 → 收尾到 deliverable
```

## 触点清单(7 处)

| 文件 | 变更 |
|---|---|
| `src/agent/graph_loop.rs` | 新增 `pending_sub_runs` 字段、`SubRunHandle`、fork/poll 方法、改写 `build_filling_hint`、`step_graph` 主循环加 polling 优先 |
| `src/agent/proposer.rs` | `ProposePatch` 加 `drill_down` 字段、`DrillDownMark` 结构、system prompt 新增 schema 段、校验规则 |
| `src/graph/mod.rs` | 不加 struct 字段(用 metadata);如有需要加 metadata key 常量 |
| `src/web/checkpoint.rs` | `Checkpoint` 加 `sub_run_links: Vec<SubRunLink>`(#[serde(default)]) |
| `src/web/persistence.rs` | 新增 `create_sub_run_dir`、`append_sub_run_link`、`read_sub_run_status`、`sub_run_run_json` 辅助方法 |
| `src/web/api_runs.rs` | 新增 `GET /api/runs/:id/sub-runs`、`GET /api/runs/:id/parent` |
| `src/web/state.rs` | `EngineConfig` 新增 `max_drilldown_depth: usize`(默认 2,即允许 3 层) |

## 不做(YAGNI)

- **多个子图不并行**。父图按 C 出现顺序串行展开子图(`pending_sub_runs` 是 map 不是 set 也可并行,但本期不实现)。
- **不改 `Contains` relation**。本期下钻走 `fork_sub_graph_for` 新机制,不用 `Contains` 在原图嵌套。
- **不引入子 deliverable 节点**。子图 done 即视为子 deliverable 完成,不需要末端子节点。
- **不重新设计 `cascade_expand.rs`**。它是 subagent 执行层下钻,与 L0 图构建层下钻是不同抽象,保持独立。
- **不预生成子图预览**。子图在父图阶段不显示节点细节,只有"C 已被下钻为子图 X"的提示。
- **不自动删 `start→deliverable` 直连边**。`build_filling_hint` 不再强制删,模型自决;冗余直连边监控是 [[2026-06-21-redundant-direct-edge-design]] 的范围,继续按那套机制跑。

## 与既有设计的关系

- [[2026-06-21-graph-direction-leadsto-design]]:`start→deliverable` 主轴仍 LeadsTo,中间节点自由关系(LeadsTo / DependsOn / Contains)。本期是这一步的"实践落地"——把 prompt 里的"单链"暗示去掉,让中间节点真正丰富起来。
- [[2026-06-21-orphan-node-check-design]]:孤儿检查机制继续工作。下钻不影响孤儿检测——子图内仍用 `replay_from_anchor`;父图在 C 等待期间不进行 C 后续节点的孤儿判定。
- [[2026-06-21-redundant-direct-edge-design]]:`start→deliverable` 冗余直连边监控继续工作。下钻不影响该机制。
- [[2026-06-21-graph-index-rebuild-on-deserialize-design]]:反序列化重建索引对子图同样适用。
