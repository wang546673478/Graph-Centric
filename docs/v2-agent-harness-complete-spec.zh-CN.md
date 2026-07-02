# Graph-Centric Agent Harness v2 完整规范

**Status:** 规范文档  
**Date:** 2026-07-02  
**Language:** 简体中文

> **本规范不只修订核心运行逻辑,还覆盖 agent 用到的所有工具 / 记忆 / UI / 其他子系统。** 分为五个主块 + 总清单 + 验证步骤。
>
> **迭代基础**:v1(commit `390d453` 之前的版本)在 Clarifying 与 Explore 上各加了一道硬门。本规范推翻这两道硬门,代之以 agent 自决 + 结构化兜底的新机制。
>
> **生效范围**:仅 Phase::Graph + GraphPhase::{Clarifying, Filling, Expanding, Verifying} 四个阶段 + 全部工具/记忆/UI 增强。
>
> **未变化部分**:Phase 状态机、6 种 step 类型、CheckpointStore、drill-down 行为、BFS 硬门、PostExecutionValidator。

---

## 0. 设计目标总览

| # | 目标 | 测量 |
|---|---|---|
| G1 | Clarifying 由 agent 自决何时结束 | 实际轮数 + 反相似触发 |
| G2 | Filling 显式编码代码任务相互依赖 | 边类型 + Kahn wave 顺序 |
| G3 | Explore 迭代到收敛(200 软上限) | iter 实际 + tier 注入触发 |
| G4 | 子代理能精准读节点而不是读整文件 | 工具调用 + token 节省 |
| G5 | 模型看到的信息是 L0 全图 + L1 范围选送 + L2 按需 | prompt 体积 + 任务表现 |
| G6 | WebUI 实时呈现图差异、阶段、迭代、子 run | 观察 |
| G7 | 其他子系统具备工业级可观测性、可调性 | 配置面板 + 仪表盘 |

---

## 1. 核心运行逻辑 v2

### 1.1 Clarifying —— 软上限 10 + 反相似

```
state machine:
  Clarifying → ask_user(1) → Paused → resume → ... → N 轮后:
    ├─ 相似度 > 0.85 + count ≥ 3   → Block
    ├─ count < 10 + 相似度 ≤ 0.85  → 允许再 AskUser
    ├─ count ≥ 10                   → 软上限 Block
    └─ agent emit ProposePatch     → phase = Seeding, count 归零
```

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `clarification_count` | u32 | 0 | AskUser 累计轮数 |
| `clarification_history` | VecDeque<String> | 容量 5 | 最近 5 轮 question |
| `clarification_max` | u32 | 10 | 软上限 |
| `clarification_similarity_threshold` | f64 | 0.85 | 反相似阈值 |

### 1.2 Filling —— 三类边编码代码任务相互依赖

| 边类型 | 含义 | 例子 |
|---|---|---|
| `DependsOn` | 类型/接口/数据结构前置 | `add cmd` DependsOn `TodoItem struct` |
| `LeadsTo` | 流程顺序 | `build runner` LeadsTo `demo verify` |
| `Contains` | 容器归属 | `data-model` Contains `storage.go` |

Kahn 出 wave,环报错回 Filling 改图。

### 1.3 Explore —— 迭代到收敛,200 软上限

| 字段 | 默认 | 说明 |
|---|---|---|
| `explore_max` | 200 | 硬上限 |
| `explore_soft_hint_at` | 100 | 软提示 |
| `explore_hard_hint_at` | 150 | 硬警告 |
| `explore_similarity_threshold` | 0.85 | 反相似阈值 |

post-Explore gate 改写:Explore 之后允许再 Explore(iter < 200 + 非相似)。

### 1.4 跨阶段不变量(本规范不修改)

- 6 step 类型不变
- drill-down 上限 2 层
- Graph 单调性 + 每次 patch bump version
- BFS 硬门 (`replay_from_anchor`)
- stagnation 三档 4/6/12
- LoopState 6 变体

### 1.5 验收 case

| Case ID | Goal | 期望行为 |
|---|---|---|
| C1 | "做一个简易的命令行 todo 工具" | agent 第 1 轮 AskUser(交付物/语言/目标三选),用户答 Go+JSON 后,agent 自评够了 → emit ProposePatch 进 Seeding |
| C2 | 用户答复反复含混 | agent 追问直到 count=10,触发"信息密度已饱和"Block |
| C3 | agent emit 相似问题 | 相似度 > 0.85 + count≥3 → 立即 Block |
| C4 | 用户答案里包含完整新约束 | 哪怕 count=8,agent 自评已收敛可立即 Seeding |
| F1 | 一个完整的 go-todo graph | 添加约 7~9 个 Task 节点,DependsOn + LeadsTo + Contains 三种边都出现 |
| F2 | 一个制造了循环依赖的尝试 | `DagScheduler::plan` 返回 `Err("cycle")`,graph loop 退回 Filling |
| F3 | 三个独立的 cmd 节点 | 在 scheduler 输出的同一 batch 并行执行 |
| F4 | 复杂 Task 节点 | agent 可以 emit `GraphPatch { drill_down: Some(...) }`,触发子 GraphLoop |
| E1 | Filling 中遇到 1 个不确定细节 | 第 1 次 Explore → agent 自评满意 → emit ProposePatch,iter 归零 |
| E2 | Filling 中遇到不确定,sub-agent 回答不够清晰 | 连续 Explore 2~5 轮,直到满意 emit ProposePatch |
| E3 | agent emit 相似 Explore 多次 | iter ≥ 3 且相似度 > 0.85 → Block |
| E4 | agent 探索同一问题到 100 轮 | 注入 soft hint |
| E5 | 同上,到 150 轮 | 注入 hard hint + last chance |
| E6 | 同上,到 200 轮 | Block("探索无收敛,需 escalate") |

---

## 2. 关系图节点精准读取工具

### 2.1 设计动机

**当前问题**:
- 子代理只有 `Bash / ReadFile / WriteFile / EditFile / WebSearch / WebFetch` 这些通用工具
- 读源码靠 `ReadFile(path)`,需要先知道 `path`
- L2(原始源码)经常被读整文件,但实际只要某个函数
- 读 graph 信息没有专门工具,只能去问 LLM 描述
- **结果**:token 浪费 + 容易越界(读不该读的文件) + 难以 audit

**新工具集** — 把 graph 作为一等公民,直接给 agent 精准的、按层级的、按范围的查询能力。

### 2.2 新增工具集

#### 工具 1:`read_graph_node`

```rust
/// 读图节点的指定层
///
/// Args:
///   node_id: NodeId              (必填)
///   layer: "L0" | "L1" | "L2"   (默认 "L1")
///   line_range: Option<(usize, usize)>  (L2 时可选)
///   depth: usize = 0             (读相邻边,0=只看自己)
///
/// Returns:
///   L0 → { node: Node, edges_in: Vec<Edge>, edges_out: Vec<Edge> }
///   L1 → L1Description
///   L2 → file content (or slice)
```

**为什么需要**:agent 想知道"owners-api 这个 Task 节点的语义"时,直接 `read_graph_node("owners-api", L1)`,不用读整文件再 grep。

#### 工具 2:`search_graph`

```rust
/// 在 L0/L1 文本里搜节点/边
///
/// Args:
///   query: String
///   search_in: "node_summary" | "l1_responsibility" | "edge_evidence" | "all"
///   node_kind_filter: Option<Vec<NodeKind>>
///   limit: usize = 20
///
/// Returns:
///   Vec<{ node_id, snippet, score }>
```

**为什么需要**:agent 想"找所有跟'费用'相关的节点",`search_graph("费用", "l1_responsibility")`,返回匹配节点列表。

#### 工具 3:`find_similar_nodes`

```rust
/// 给定节点,找 graph 里最相似的 N 个
///
/// Args:
///   node_id: NodeId
///   similarity_to: "L0_summary" | "L1_responsibility" | "L1_full"
///   top_k: usize = 5
///   threshold: f64 = 0.7
///
/// Returns:
///   Vec<(NodeId, f64)>
```

**为什么需要**:Clarifying/Explore 反相似检测用,`find_similar_nodes("own question", "L0_summary", 5, 0.85)`。

#### 工具 4:`trace_dependency`

```rust
/// 沿某关系回溯/前瞻
///
/// Args:
///   start: NodeId
///   relation: RelationType
///   direction: "upstream" | "downstream" | "both"
///   max_depth: usize = 5
///
/// Returns:
///   Vec<Vec<NodeId>>  (路径列表)
```

**为什么需要**:agent 想"看 owners-api 的所有 DependsOn 链",`trace_dependency("owners-api", DependsOn, upstream, 5)`,返回依赖图。

### 2.3 与现有 ReadFile 的关系

| 场景 | 用 | 不用 |
|---|---|---|
| agent 想知道某节点 L1 语义 | `read_graph_node(id, L1)` | ~~ReadFile(整文件)~~ |
| agent 想看具体源码 | `read_graph_node(id, L2)`(用 path 自动解析) | `ReadFile(path)` |
| agent 想 search 概念 | `search_graph("费用")` | ~~grep 整个 repo~~ |
| agent 想 audit 关系 | `trace_dependency(id, Dep, up)` | ~~手动 walk edges~~ |

**保留 `ReadFile`** 用于"我想读这个具体文件,与 graph 无关"的情况,例如读 README。

### 2.4 ScopeGuard 协同

`read_graph_node(id, L2)` 内部:
1. 解析 `id` → `Node.path`
2. 调 `ScopeGuard::check_read(path)`
3. 拒绝越界访问

**效果**:子代理"读哪个节点"本身就受 ScopeGuard 限制,无法绕过去读 scope 外的文件。

### 2.5 验收 case

| Case ID | Goal | 期望 |
|---|---|---|
| T1 | 子代理在 owners-api 任务上工作 | 调用 `read_graph_node("owners-api", L1)` 拿到语义,而不是 `ReadFile(整文件)` |
| T2 | agent 想找"费用"相关 | `search_graph("费用", "l1_responsibility")` 返回 1~3 个节点 |
| T3 | Clarifying 反相似触发 | 调 `find_similar_nodes(question, ..., 5, 0.85)` 返回 ≥ 1 个高分匹配 → Block |
| T4 | 子代理想读不在 scope 内的文件 | `read_graph_node(id_out_of_scope, L2)` 被 ScopeGuard 拒绝 |
| T5 | 想看 owners-api 依赖链 | `trace_dependency("owners-api", DependsOn, upstream, 5)` 返回 Vec<Vec<NodeId>> |

---

## 3. L0 / L1 / L2 三层记忆选择性输送

### 3.1 设计动机

**当前问题**:
- proposer / verifier / reviewer / decomposer 各自手工拼 prompt
- 把整个 graph 序列化塞进 prompt 经常超 token 限制
- L2(原始源码)从来不在主代理 prompt 里(主代理 L0/L1 only),但子代理处理大文件时塞太多
- L1 没被精准按"当前步骤相关"裁剪

**改造方向**:引入 `ContextBuilder` 统一管理"图里哪些内容这次进 prompt"。

### 3.2 ContextBuilder 架构

```rust
pub struct ContextBuilder {
    graph: Graph,
    scope: NodeIdScope,           // 当前步骤相关的节点
    token_budget: TokenBudget,    // 预算上限
}

pub struct BuiltContext {
    l0_summary: String,           // 必送:整个图的结构 summary
    l1_for_scope: String,         // 选送:scope 内节点的 L1
    l2_references: Vec<NodeRef>,  // 引用:按需,只发"file:line"指针
    notes: Vec<ContextNote>,      // 附注:已知冲突、orphan 提示等
}

impl ContextBuilder {
    pub fn new(graph: Graph) -> Self;
    pub fn with_scope(mut self, scope: NodeIdScope) -> Self;
    pub fn with_budget(mut self, tokens: usize) -> Self;
    pub fn build(&self) -> Result<BuiltContext>;
}
```

### 3.3 L0 必送策略(全图结构 summary)

- 格式:`{nodes_count} nodes, {edges_count} edges`
- 列出所有节点 id + kind + 一句话 summary
- 列出所有边(源、目标、关系)
- **始终全量**(L0 是图骨架,信息密度极高,必须可见)
- 压缩技巧:大图 > 100 节点时切到 "summary block" 形式,只列 id 列表 + counts,详情按节点 id 引用

### 3.4 L1 选送策略(scope 过滤)

`scope` 怎么来:
- Filling 中:scope = 当前 step 的 `involved_nodes`(proposer 自带)
- Task phase:scope = sub-task 的 `involved_nodes`(dispatcher 注入)
- Review phase:scope = 全部 nodes(review 需看全图)

按 scope 过滤 L1:
- `scope.nodes` 的 L1 全送
- `scope.neighbors`(1 跳可达)L1 送
- 其他节点 L1 折叠成 `[...] out-of-scope, use read_graph_node(id, L1) to inspect`

### 3.5 L2 按需策略

**默认不送**。L2 只在以下情况出现:
- model 显式调 `read_graph_node(id, L2)`
- 或者 verifier 在做 L1 drift 校验时,自动 fetch L2(单独通道,不进主 prompt)

输出里 L2 永远**只放引用**:
```
[reference] /src/owners/api.go:23-67   (use read_graph_node("owners-api", L2) to view)
```

不直接贴源码。

### 3.6 Token 预算管理

`ContextBuilder::with_budget(N)`:
- L0 优先
- L1 排序:scope 内 > scope 邻居 > 远端
- 超预算时按"远端 L1 → scope 邻居 L1 → scope 内 L1 详细"顺序截断
- L2 永不进(只发引用)

每个模型层有自己默认预算:
| 层 | 预算(tokens) | 备注 |
|---|---|---|
| proposer | 8000 | L0+L1 scope |
| verifier L1 | 4000 | 节点子集 |
| verifier graph | 2000 | summary 形式 |
| reviewer | 6000 | 全文 |
| decomposer | 4000 | task 子图 |
| subagent | 12000 | 任务相关 + L2 按需 |

### 3.7 验收 case

| Case ID | Goal | 期望 |
|---|---|---|
| M1 | Filling 中 proposer 看 5 节点图 | prompt 包含 L0 全图 + 5 个 L1 完整,体积 < 8K tokens |
| M2 | 大图(100+ 节点) | L0 用 summary 形式,详细按需 |
| M3 | verifier 做 L1 drift | 自动 fetch 对应 L2 做 diff,不影响主 prompt |
| M4 | subagent 处理大文件 | L2 引用形式,不内联 |
| M5 | 显式超预算 | 远端 L1 截断,scope 内 L1 保留,可见附注说明 |

---

## 4. WebUI 优化

### 4.1 实时图形差异可视化

**问题**:用户看到图在 Filling 中不断变,但不知道"这一轮加了什么 / 删了什么"。

**改造**:
- 每个 patch 落地后,前端计算 `graph_diff(old, new)`
- 高亮新增节点(绿色)、新增边(蓝色)、删除节点/边(红色)
- 动画过渡 1.5s,然后渐隐到稳定状态
- 鼠标悬停某节点 → 显示"added in round N by proposer / repairer / cascade"

### 4.2 阶段进度指示

**问题**:用户不清楚 agent 在哪个阶段(Clarifying / Filling / Verifying / Task / Review)。

**改造**:
- 顶部固定状态条:`[Graph:Filling] round 8 / stagnation 2`
- 阶段切换时高亮 + 简短说明
- Filling 子状态条:`patches 12 | explores 4 | drilldowns 1`
- 颜色:Clarifying(蓝)/ Filling(绿)/ Verifying(黄)/ Task(紫)/ Review(橙)/ Done(灰)

### 4.3 Block 状态交互

**问题**:Block 后用户不知道"现在该干嘛"。

**改造**:
- Block 时弹 modal:
  - 标题:为什么 Block
  - 内容:触发原因(反相似/反空/反依赖)
  - 选项:
    - (a) 提供更明确的答复(打开 input)
    - (b) 强制让 agent 进入下一阶段(emit ProposePatch)
    - (c) 中止 run
- 用户选 (b) 时,UI 推送"强制 Seeding"指令到后端

### 4.4 探索迭代可视化

**问题**:Explore 200 软上限时,用户看不到 agent 探索到哪里了。

**改造**:
- Filling 阶段时,显示"explorer 进度条":`[####------] 4/200`
- 颜色:0~50 绿、50~100 黄、100~150 橙、150~200 红
- 每次 Explore 后,显示上一轮 question + answer 摘要(避免 token 爆炸,用 L1 压缩)
- 类似 progress + log 形式

### 4.5 子 run 面板

**问题**:drill-down 子 run 散在文件系统里,前端要展示父子关系。

**改造**:
- 主 run 详情页右侧:子 runs 列表(树状)
- 每个子 run 显示:父节点 id、当前状态、checkpoint 计数、agent 当前阶段
- 点击展开 → 跳到子 run 独立视图(同主 run 模板,只是数据源不同)

### 4.6 检查点时间线

**问题**:checkpoint 推了 30+ 个,用户想"看看 round 5 时的图长什么样"。

**改造**:
- 主 run 详情页底部:checkpoint timeline
- 滑块可拖到任意 round
- 显示该 round 的:graph snapshot、当时 conversation transcript 摘要、当时阶段
- 对比按钮:选两个 round,Diff 视图(graph diff + transcript diff)

### 4.7 实时连接稳定性

**问题**:WebSocket 断线时,前端丢消息。

**改造**:
- 断线时,前端自动重连(指数退避)
- 重连成功后,请求"since event N"补齐丢失事件
- 后端用 `Last-Event-ID` 机制记录 event log

### 4.8 验收 case

| Case ID | Goal | 期望 |
|---|---|---|
| U1 | Filling 中观察图 | 每次 patch 后高亮 diff,1.5s 后渐隐 |
| U2 | 任意时刻 | 顶部状态条显示当前阶段 + round + stagnation |
| U3 | Block 触发 | modal 弹出,3 选项可见 |
| U4 | Explore 进行中 | 进度条实时更新,轮 question/answer 摘要可见 |
| U5 | 主 run 有 1+ 子 run | 右侧子 runs 面板列出,可点击跳转 |
| U6 | 主 run 有 30+ checkpoint | timeline 滑块可拖到任意 round,显示 snapshot |
| U7 | 断网 30s | 自动重连,补齐丢失事件 |

---

## 5. 其他模块优化方向

### 5.1 Tool 层

| 改造 | 描述 |
|---|---|
| `read_graph_node` 等 4 个新工具 | 见 §2 |
| `Bash` 加 dry-run 模式 | 复杂命令先试运行,确认无破坏 |
| `EditFile` 加 graph-aware 模式 | 写入时校验"目标 path 是否在 graph 里" |
| `WebSearch` 加缓存层 | 同 query 不重复发,默认 TTL 1h |
| `WebFetch` 加 max-bytes 限制 | 防止大页面塞爆 context |

### 5.2 Model 层

| 改造 | 描述 |
|---|---|
| Token 用量实时统计 | 每个 run 的 prompt/completion token 流式上报 |
| 响应缓存 | 同 prompt + 同温度 → 复用响应,降低 token 消耗 |
| 流式 fallback 透明化 | `ModelWithEvents` 已有,扩到支持自定义 chunk size |
| 多模型路由 | proposer 用便宜模型,reviewer 用贵模型,config 配 |
| Reasoning 模型适配 | `text_or_reasoning()` 已有,扩到子代理 |

### 5.3 Skills

| 改造 | 描述 |
|---|---|
| 技能触发精准度 | 现在 Jaccard 0.25 偏松,改 0.4 + L1 语义相似度补充 |
| 技能执行可视化 | 命中技能时 UI 显示"用 X 技能" |
| 技能质量评分 | capture 时记录"用了多少次 / 成功率",低于阈值自动归档 |
| 技能反注入 | LLM-as-judge 判定技能 quality,差技能不进入匹配池 |

### 5.4 Scheduler

| 改造 | 描述 |
|---|---|
| Wave 可视化 | `Schedule { batches }` 用甘特图展示在 UI |
| 动态批大小 | 重要任务独占一波,轻任务并批 |
| 优先级标注 | Task 节点加 `priority: u8`,调度时优先高优 |
| 资源感知 | 监控 GPU/CPU 占用,自动 throttle |

### 5.5 HeartBeat

| 改造 | 描述 |
|---|---|
| 启动 dashboard | 进度轮数 / 成功率 / 当前任务 |
| 失败模式分类 | stagnation / cycle / unknown,各对应不同下一轮 prompt |
| 慢启动 | 第一次跑用 verbose,后续精简 |
| 人工干预通道 | HeartBeat 跑中可注入 hint 或中止 |

### 5.6 持久化

| 改造 | 描述 |
|---|---|
| 增量保存 | checkpoint 增量写,不全量重写 |
| 压缩 | checkpoint 历史超过 100 时,老 checkpoint 压缩成 summary |
| 备份 | 关键 run 自动备份到第二位置 |
| 清理策略 | 失败/被遗弃 run 30 天后归档,1 年后清理 |

### 5.7 失败归因

| 改造 | 描述 |
|---|---|
| 错误分类器增强 | `PostExecutionValidator` 已有,扩到 5+ 种语言的 pattern |
| 失败路径 | `Error` 走 4 种:GraphError / TaskFailure / Stagnation / Block,各对应不同 user 提示 |
| 重试策略 | 临时失败(网络)重试,逻辑失败(graph 错)不重试 |
| 修复建议 | 失败时显示"建议:补一条 X→Y 边" |

### 5.8 Token 成本跟踪

| 改造 | 描述 |
|---|---|
| 每 run token 用量 | prompt + completion 分开计 |
| 每 step token 用量 | 哪个 step 最贵,优化目标 |
| 预算限制 | 设 token 上限,超过时暂停并提醒用户 |
| 成本 dashboard | 总览:本月总 token / 主流向 / 异常增长 |

### 5.9 多 run 比较

| 改造 | 描述 |
|---|---|
| Fork 视图 | 选一个 checkpoint,显示"从这里能 fork 几个分支" |
| 对比模式 | 选两个 run,Diff 图 + Diff transcript |
| 模式识别 | "run A 探索 5 轮收敛,run B 探索 30 轮,差在哪?" |
| 复用跨 run | run A 的 sub-task 直接拿来给 run B 用 |

### 5.10 验收 case

| Case ID | Goal | 期望 |
|---|---|---|
| X1 | 复杂 run 完成后 | 工具调用、token 用量、scheduler wave、心跳、持久化都有可视化数据 |
| X2 | 失败 run | 错误归因清晰,修复建议可见 |
| X3 | 跨 run 比较 | 选 2 个相似 run,Diff 一目了然 |

---

## 6. 实施总清单(按文件)

| 文件 | 主要改动 | 优先级 |
|---|---|---|
| `src/agent/proposer.rs` | 删 MAX 2 硬限、post-Explore gate 改写、相似度检测 API | P0 |
| `src/agent/graph_loop.rs` | 加 4 个新字段、新增 saturation 检查、stagnation 扩 explore tier | P0 |
| `src/agent/graph_loop.rs::GraphLoopConfig` | 7 个新 config 字段 | P0 |
| `src/tools/mod.rs` | 注册 4 个新工具(read_graph_node / search_graph / find_similar_nodes / trace_dependency) | P0 |
| `src/tools/scope_guard.rs` | 给 read_graph_node 加重定向的 path 校验 | P0 |
| `src/context/` (新) | ContextBuilder、BuiltContext、TokenBudget、scope 过滤 | P0 |
| `src/agent/{proposer,verifier,reviewer,decomposer,subagent}.rs` | 改用 ContextBuilder 替代手写 prompt | P1 |
| `src/model/mod.rs` | token 用量统计、响应缓存 | P1 |
| `src/skills/matcher.rs` | 阈值 + L1 语义补充 | P2 |
| `src/scheduler/mod.rs` | wave 优先级 + UI 暴露 | P2 |
| `src/web/api_runs.rs` | 推 checkpoint / 阶段变化事件 + WebSocket 协议 | P1 |
| `src/web/checkpoint.rs` | 增量保存 + 压缩 | P2 |
| `src/web/state.rs` | 7 个新 config 字段穿透 | P0 |
| `src/web/persistence.rs` | 备份 / 清理策略 | P3 |
| `webui/src/components/...` | 5 个新组件:GraphDiff / PhaseProgress / BlockModal / ExplorerBar / SubRunTree / CheckpointTimeline | P1 |
| `webui/src/composables/useWebSocket.ts` | 断线重连 + Last-Event-ID 补齐 | P1 |
| `webui/src/views/RunView.vue` | 集成 5 个新组件,新 dashboard 页 | P1 |
| 测试 | 单元 + 集成 case,对应 §1.5 / §2.5 / §3.7 / §4.8 / §5.10 验收 case | P0 |

---

## 7. Goal-driven 验证步骤

启动一个标准 goal:
```
"做一个简易的命令行 todo 工具 (Go 单文件,add/list/done,JSON 存储)"
```

观察并对照:

### 7.1 核心逻辑 §1

| 观察项 | 期望 |
|---|---|
| Clarifying 轮数 | ≤ 2(agent 自决) |
| 反相似触发 | 用户答得极端模糊时,触发"重复追问"Block |
| Filling 节点边类型 | DependsOn + LeadsTo + Contains 三种 |
| Explorer iter | < 200,正常收敛 |

### 7.2 工具 §2

| 观察项 | 期望 |
|---|---|
| 子代理读节点 | 调 `read_graph_node(id, L1)`,不是 `ReadFile(整文件)` |
| 反相似 | `find_similar_nodes` 命中阈值,Block |
| 越界 | 子代理试图读 scope 外节点,被拒 |

### 7.3 记忆 §3

| 观察项 | 期望 |
|---|---|
| proposer prompt 体积 | < 8K tokens |
| L0 必送 | 全图结构可见 |
| L1 选送 | scope 内完整,scope 外折叠 |
| L2 | 只发引用,不内联 |

### 7.4 WebUI §4

| 观察项 | 期望 |
|---|---|
| 阶段进度 | 顶部状态条显示 |
| 实时 diff | 新增节点/边高亮 |
| Block 弹窗 | 3 选项可见 |
| Explorer 进度条 | 实时更新 |
| 子 run 面板 | 列出 + 可点击 |
| Checkpoint timeline | 滑块可拖 |

### 7.5 其他 §5

| 观察项 | 期望 |
|---|---|
| Token 用量 | 每 step 可见,总用量可看 |
| 失败归因 | 错误分类 + 修复建议 |
| Wave 可视化 | 甘特图可见 |
| 跨 run 比较 | 选 2 个 run,Diff 可见 |

---

## 8. 文档维护约定

- **§1 核心逻辑**:任何 step 类型 / FSM 改动需更新
- **§2 工具**:新增工具需追加,旧工具 deprecate 需标记
- **§3 记忆**:ContextBuilder 字段调整需同步
- **§4 UI**:任何新组件需补验收 case
- **§5 其他**:每模块新功能需补验收 case
- **§6 清单**:每个文件改动一次,清单更新一次
- **§7 验证**:新增验收 case 时,跑一遍 goal 验证一遍

---

## 9. 一句话总结(完整版)

> **v2 不只是把硬上限换成"agent 自决 + 结构化兜底",而是从"模型看到啥就处理啥"升级成"模型只在精准的、按层的、按 scope 的视图里行动":核心 FSM 释放 agent 自由(Clarifying 10、Explore 200、反相似兜底);agent 工具有了 graph-aware 版本(4 个新工具);记忆输送有 ContextBuilder 统一管 L0 必送 + L1 选送 + L2 按需;WebUI 实时呈现 diff / 阶段 / Block / 探索 / 子 run / checkpoint;其他模块补全工业级可观测性。**
