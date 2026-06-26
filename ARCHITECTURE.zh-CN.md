# Architecture / 架构

本文档解释设计的 **"为什么"**——那些从代码里看不出来的决策、我们考虑过
又否决的备选、以及塑造每个组件的权衡。如果还没读过 `README.md`，先读那个。

**其他语言：** [English](ARCHITECTURE.md) | 简体中文

---

## 近期变更

- **v2.6（2026-06-26）— 全员 tool_calls 迁移。** 7 个模型层（Proposer、
  Decomposer、L1Enricher、L0+ScopeGap Repairer、Verifier L1+graph
  self-checks、Reviewer、CascadeBacktracker）现在都声明 OpenAI
  `tool` schema 并优先用 `tool_calls` 而不是文本 JSON 解析。触发事件
  是生产 run db2d993d：DeepSeek-v3 / MiniMax M3 的 reasoning-only
  响应（`content` 为空，JSON 在 `reasoning_content` 中）把 decomposer
  杀掉，错误是 `proposer: no '{' in response`。修复：新增
  `ModelResponse::text_or_reasoning()` 辅助方法，把 `content` →
  `reasoning_content` 的兜底集中；每一层用
  `parse_*_from_tool_calls(&[ToolCall]) -> Option<T>` → 文本兜底
  的模式包起来。`StreamDelta` 新增 `ToolCallArgument` 变体用于实时
  按片段流式 tool_call；`RunEvent::StreamToolCall` 是其线协议。测试
  从 477 增至 606。主 agent 和 sub-agent 仍然用**不同**协议——见 §5
  看完整的"有意收窄"故事。见 `CLAUDE.md` 看 22 条核心思想，其中
  #5、#14、#15、#18、#22 捕获了相关的子故事。

- **v2.5（2026-06-20）— Intake gate 现在用代码强制。** `intake.rs` 新增
  `classify_task_clarity`（启发式：EN+ZH 模糊起点短语、短无动词、单词）
  和 `check_intake_compliance`（对模糊任务在 round 0 上拒绝非
  `ask_user` 步骤）。§1a 中"未来可以加确定性检查"的注释已经过时——
  检查已存在，在每一步都跑，并通过 fix-it retry 路径把错误反馈给
  模型。Mode A vs Mode B 不再只靠 prompt。

- **v2.4（2026-06-18）— 流式输出与持久化。** `ModelWithEvents` 透明
  包装任意模型——`complete()` 调用被路由到 SSE 流式传输
  （`complete_stream()`），每个 token 实时作为 `StreamChunk` 事件
  转发给所有 WebSocket 客户端。`RunPersistence` 让 run 跨进程重启
  存活：元数据、checkpoint 和分支记录以 JSON 形式存入
  `data/runs/<id>/`，启动时重新加载。见 §13 获取流式架构和持久化布局。

- **v2.3（2026-06-17）— HeartBeat 自改进循环。** 自治多轮 agent 优化，
  跨进程重启存活。启动时，若存储了 heartbeat task，自动创建新 run。
  每次 Review 成功后，轮次计数 +1，变更已提交，二进制被重编译，进程
  退出——外部启动器重启它继续下一轮。通过 `POST /api/heartbeat` 或设置
  页面管理。见 `docs/implementation-plan-fractal-graph.md`。

- **v2.2（2026-06-17）— Hook 系统。** `PreToolUse`/`PostToolUse` 生命周期
  回调环绕每次工具调用。搭载 `LoggingHook`（tracing）、`StatsHook`
  （按工具统计调用次数/耗时/字节数）和 `SafetyHook`（拒绝模式匹配）。
  集成到 SubAgent 的 `ToolContext`。见 `src/tools/mod.rs`。

- **v2.1（2026-06-16）— 分形架构。** L0/L1/L2 递归：复杂节点（文件、函数）
  展开为带有同样三层的子图。AST scanner 提取函数/类级别符号，带行号
  范围作为子节点通过 `Contains` 边连接。质量指标（LOC、unwrap/unsafe/
  TODO 数量）按文件计算。3D 图面板基于 Three.js。见
  `docs/recursive-l0-l1-l2-architecture.md`。

- **v2.0（2026-06-15）— 级联回溯。** 下游节点失败时，主 agent 自动重规划，
  级联验证沿着所有前驱边一直走到不可变的锚节点。见
  `docs/design-v2-cascade-backtrack.md`。

- **v1.1（2026-06-03）— IMPLICIT_CWD_WRITE_VERBS。** 见 §5、§10、§13。
- **v1（2026-06-03）— Tool System Rework。** 见 §10、§7。

---

## 1. 核心论点：长任务上 binding-constraint 在 harness

对长跑 LLM agent 的实证工作（SWE-agent、Terminal-Bench、Anthropic
的 orchestrator-workers 写法）收敛到同一个观察：**任务一旦拉长，
系统的性能瓶颈在 harness 上，不再是模型的推理能力。** 把模型固定，
单靠 harness 就能把基准分数提升一个数量级。

现在大多数 harness 都是某种 ReAct 循环的变体——"模型想一下，模型调个
工具，模型再想一下"。这在短的、本地范围的任务上能跑。长任务上缩放
性差，因为：

- **上下文漂移。** 每次工具结果以消息形式落进对话。20 轮后，模型
  是在基于自己过去的工具输出做推理，而不是在推理真实世界。
- **错误悄无声息地叠加。** 第 3 轮的一个小误解会塑造之后每轮，到第
  15 轮才以具体的失败浮出水面。届时直接原因已经被埋了。
- **协作是隐式的。** 子 agent（如果存在的话）通过拼接消息共享上下文。
  它们会漂移开。

我们的赌注是：**关系图作为共享基底**能反向治这三个失败模式。图是每
个组件读写共用的世界模型；对话变成注解，不再是状态。

## 1a. 核心思想：图是 orchestrator 的计划

图**不是**被动数据存储,**也不是**事件流水。**图是 orchestrator 的
计划**:大模型(作为主 agent)把图当成工作记忆来维护,整套循环是:

1. **规划(Plan)。** 主 agent 读图,然后
   - **Mode A(明确方案)** — 给范围清晰的任务加子节点;**或者**
   - **Mode B(探索形式)** — 在画任何节点之前,先发 `ask_user` 跟用
     户对齐方向。

   两种模式里,主 agent 在这一阶段都**不亲自干活** — 只写节点/边。
   任何需要子领域调研的活,是 Task 阶段子 agent 的事。
2. **派发(Dispatch)。** 子 agent 各自负责一个子节点,以图(加上子节点
   的 spec / 证据)做上下文执行。子 agent **不直接改图** — 只回报
   `success` 或者 `report_graph_error` + 证据。
3. **复核(Review)。** 对每个子节点按 orchestrator 的 spec 做 per-node
   复核。通过 → 子节点标 done;不通过 → orchestrator 写一个**局部**
   `GraphPatch`(只动那一个子节点的 spec,不是整张图),循环重派那一
   个子 agent。

"graph-centric" 在代码里的含义:**每一次状态变更都是一个有明确 scope
的 `GraphPatch`,无论触发源是什么,都走同一个机制**。`LocalRepairer`
(处理 verifier 发现) 和 per-sub-agent-failure 重派,走的是**同一套机
制** — 触发源不同,patch 的形状一样。

Mode B 不可省。如果模型可以从一个模糊任务里直接画图,它会**挑一个
解读**,而这个解读就是后面整段循环唯一能看见的东西。24 轮 Graph phase
里没有"猜错第一步"的恢复路径 — verifier 和 sub-agents 都会基于一个
框错的计划工作。Mode B 在任何规划**之前**强制做一次澄清。

具体落地上,Mode B 由 system prompt **和** `intake.rs::check_intake_compliance`
里的确定性 gate 一起强制:一个新对话里的第一个 `ProposerStep` 会
对照 `classify_task_clarity(task)` 做检查。如果任务是 Vague
（启发式:EN+ZH 模糊起点短语、短无动词、单词）,而步骤不是 `AskUser`,
gate 就用 `HarnessError` 拒掉,通过 fix-it retry 路径把错误反馈给
模型。system prompt 也教 Mode A/B,但仅靠 prompt 不构成 load-bearing
约束——启发式 gate 是第二道防线。分类器倾向于放行（假阳性 = 烦人的
`ask_user`;假阴性 = 浪费一次 run 在从模糊意图上建出来的图上）。

---

## 2. 三层图（L0 / L1 / L2）

图是分层的，让结构、语义和原始内容可以**独立地**被验证和修订。

### 分层

| 层 | 是什么 | 为什么要分开 |
|---|---|---|
| **L0** | 节点 + 边（结构） | 扫便宜，diff 便宜，组件之间传输便宜 |
| **L1** | 每节点 `{responsibility, implementation, design_intent, constraints}` + confidence | 语义理解贵（每节点一次 model call）但体积小；跟 L0 分开让我们能版本化、验证、重新补全 |
| **L2** | 原始字节——源文件、配置、schema、dataset | 大；永不存进图。按需通过 `SourceLoader` 访问 |

### 为什么不是单层？

我们（脑里）试过把所有内容塞进一个富节点模型。两个问题判了它死刑：

1. **缓存失效。** 改任何字段（比如节点的 L1 描述）就意味着重序列化整
   个节点——包括它的 L2 payload。在按距离构造上下文的情况下，这
   变成每个 patch 都触发大量重渲染。
2. **验证的不对称。** L0 可以结构化检查、无需 model call。L1 需要
   model 对 L2 采样。L2 本身就是 ground truth。把它们混在一起就模糊
   了"哪个检查在什么时候触发"。

### 为什么是这特定的三层？

L0 是被逼的——图就是节点 + 边。L1 = "肌肉" 是 v2 重设计时发现的层：model
输出原本分散在 `Node.summary`（一句话）和各种临时位置，从未结构化。
把 L1 强制成有类型的 `{responsibility, implementation, design_intent,
constraints}` 给了我们：

- L1-sampling 验证器的目标（"这个和 L2 还对得上吗？"）
- 修复切入点（`GraphError::L1Semantic` 路由到重新补全）
- 按距离做上下文压缩的有意义单位（d=0 完整 L1，d=1 摘要，d=2+ 单行）

L2 不进图是因为文件内容不稳定且大。按需读让图保持小，并迫使 harness
去想 L2 实际什么时候需要（结论：比你想象的少，一旦 L1 够好）。

### 跨层触发链

```
L0 patch 加节点 → 自动触发 L1Enricher 处理新节点
L1 条目的 confidence 偏低 → 重新跑补全
L2 变化（子 agent 改文件）→ 理想触发 L0 增量扫 + L1 刷新（Phase 5 — 还没做）
```

`GraphLoop::auto_enrich` 强制 L0→L1 联动。L2→L0 是被动的：下一轮
的 Verifier 或 PostExecutionValidator 注意到漂移，把它作为
`GraphInvalid` 冒出来供修复。

---

## 3. 状态机：为什么是固定 FSM，不是动态工作流

Claude Code 用 **动态工作流**——模型写 Python 一样的控制流，harness
执行。这灵活但两件事让我们选了固定状态机：

1. **主干的确定性。** 固定 FSM 下，每个转移只有一条代码路径。调试
   的时候，读三个 `step_*` 方法就知道能发生什么。动态工作流下，
   模型的运行时选择就是控制流——意味着"调试 agent"和"调试模型"变
   成了同一个问题。
2. **主干不需要 model。** 状态机可以完全无 model 跑（见
   `structural_only` verifier、`AlwaysPasses` validator 等）。这让
   测试快，让信任边界清晰：主干是经过验证的 Rust 代码；叶子是我们
   容忍不确定性的 model 调用。

权衡：表达力更弱。我们不能让模型在任务中途发明新的多步协调策略。
我们赌的是"失去那种灵活性"的成本小于"失去主干确定性"的成本。

### 三个阶段

| 阶段 | 拥有 | 出口 |
|---|---|---|
| Graph | 构建/修复 L0+L1 关系图 | Task（verifier 通过）、self（修复）、GraphInvalid（verifier 卡死） |
| Task | 任务分解 + 子 agent 派发 | Review（全部成功）、GraphInvalid（子 agent 上报）、GraphInvalid（PostExecutionValidator）、TaskFailed（子 agent 代码层失败） |
| Review | 最终验收门 | Done（通过）、GraphInvalid（judge 标记 graph/scope）、Done 附 fail verdict（judge 标记 task） |

这些不是子阶段；每个都是 `step()` 的一个独立节拍。调用方可见的
`LoopState` 反映这一点——`Running` 在 machine 处于阶段中时持续 tick，
状态机要调用方做某事时用命名状态返回。

**入口（intake）发生在 Graph 阶段内部,任何节点被画之前。** 一个新对话
里的第一个 `ProposerStep`,在任务开放到多种解读时(对应 §1a 里的
Mode B)应该是 `AskUser`。清晰、范围明确的任务直接进入 `ProposePatch`
(Mode A)。Intake 这一区分是 **prompt 约定,不是代码闸** — 详见
§1a 为什么这重要以及我们接受的 trade-off。

### 为什么 `step()` 是可重入的

`Paused { question }` 或 `GraphInvalid { errors }` 在每次调用 `step()`
时都会返回，直到调用方通过 `resume(...)` 或 `resume_with_repaired_graph(...)`
解决。重复调用不推进 machine；只是把挂起状态重新浮出来。这让调
用方的事件循环变得平凡：在循环里调 `step()`，按 variant 分发，忘掉
顺序和线程安全。

替代方案（一次性返回 + 调用方要记 context）会把状态机意识推到每
个调用方里。可重入的浮现保持契约窄。

### 为什么 GraphLoop 是纯被动的

`GraphLoop` 从不读 stdin，从不开浏览器，从不在向调用方浮出
`GraphInvalid` 时自己调用 repairer。所有外部交互都通过 `resume_*`
发生。这意味着：

- 同一个 `GraphLoop` 适用于 CLI（Demo A）、web service、自动化 CI
  harness、测试夹具、LSP server 等等
- 每个外部交互都是一个带 payload 的离散事件，在 transcript 里看
  起来很清晰

代价：调用方有更多编排代码（Demo A 的自动修复循环约 50 行）。收益：
那段编排是**可见的**、可替换的。Phase 4 加 Demo A 的自动修复时没
碰 `GraphLoop` 本身。

---

## 4. 组件分离

名字看起来冗余，直到你追出每个组件验证什么、什么时候验证：

### `Verifier`（Graph 阶段）

- **何时**：每次 `ReadyForVerify` 转移；每个 `LocalRepairer` patch
  之后也会重跑
- **做什么**：图是否抓住了任务
- **层**：结构化（确定性）+ model 自检 + L1 采样
- **输出**：带结构化 issues 的 `VerificationResult`；高严重度阻塞，
  中/低严重度只是浮出来

### `LocalRepairer`（Graph 阶段，内部）

- **何时**：在每个高严重度 issue 上被 `Verifier` 循环调用
- **做什么**：一个 issue → 一个 scope-bounded `GraphPatch`
- **纪律**：不能动 `issue.scope ∪ neighborhood ∪ patch.add_nodes`
  之外的节点。校验拒绝越界
- **三条路径**（按 `GraphError` variant 分发）：L0Structural → 读 L2
  + 提 L0 patch；L1Semantic → 调 `L1Enricher` 重写 L1；ScopeGap →
  提新节点/边来填补缺失区域

### `PostExecutionValidator`（Task 与 Review 之间）

- **何时**：可选，dispatcher 返回后、Review 之前触发
- **做什么**：确定性检查（如 `cargo check`、`pytest`），跑产出
  物并解析输出
- **三种裁定**：`Passed` → Review；`FailedAsGraphIssue` → 冒
  `GraphInvalid { source: PostExecutionValidation }` 并**跳过**
  Review；`FailedAsTaskIssue` → 继续到 Review（让 LLM judge 处理）
- **为什么在图问题时短路 Review**：原因已被确定性信号证明时，省
  一次 model 调用

### `Reviewer`（Review 阶段）

- **何时**：终态验收门
- **做什么**：对"这次跑是否完成了原始任务"做整体裁定
- **层**：确定性 backstops（图一致性、子 agent 成功、last_verification
  状态）+ LLM-as-judge 标记 `RootCause::{GraphIssue, TaskIssue,
  ScopeIssue}`
- **路由**：通过 → Done；Graph/Scope 失败 → 冒 GraphInvalid 回 Graph
  阶段；Task 失败 → Done 附 fail verdict（调用方决定是否重试 Task
  阶段）

### 为什么是这四个？

每个在不同时机、拿不同输入、花不同成本跑：

```
Verifier:          每轮 Graph 阶段跑，看 图+任务+对话     (便宜到中等)
LocalRepairer:     每个高严重度 issue 跑，看 scope+L2       (中等)
PostExecValidator: 每次 Task 阶段完成跑一次，看确定性测试 (便宜，可短路)
Reviewer:          每次 Review 阶段跑一次，看 图+结果+任务  (贵 — LLM judge)
```

合并任何两个要么丢便宜的信号（确定性 validator 的 pattern-match），
要么更频繁地跑贵信号（Reviewer）。每一层的成本匹配它的战略角色。

---

## 5. 子 agent 执行

### 三层协议，三种不同宽度（故意）

系统在三处用了不同协议。这是 v2.6 全员 tool_calls 迁移之后的局面
（见 近期变更）：

| 层 | 协议 | 宽度 | 为何要窄 |
|---|---|---|---|
| **主 agent**（Proposer） | OpenAI `tool_calls`（6 种 step 类型）| 宽 | 编排面——需要灵活性来支持模型的创造性工作 |
| **子 agent**（Dispatcher 内） | 自定义 JSON-action（`use_tool` / `final_answer` / `report_graph_error`）| 窄 | 约束执行环境（`max_steps=8`，无直接图访问）；窄 = 容易验证 |
| **Skill 编译**（`skills::compiler`）| `NodeKind::Task` + `DependsOn` 边 only | 更窄 | Skill 被缓存、回放、信任；窄 = 安全的缓存 |

看到这种不一致，第一个冲动是**统一**——让子 agent 也用 `tool_calls`，
让 skill 也输出完整 `GraphPatch`。**别这么做。** 每次收窄都是
defense-in-depth 决策：边界处协议越窄，模型在该层失控时爆炸半径
越小。主 agent 是宽的那个，因为创造性工作发生在那里；它里面所有
东西都在轨道上。如果将来有贡献者提议统一协议，问题只有一个：
**我们会丢掉什么安全保证？**

### 为什么子 agent 用 JSON-action，不用原生 tool_calls

DeepSeek（以及 OpenAI、Anthropic 等）都支持原生的 function-calling
——模型在响应里发出结构化的 `tool_calls`，运行时通过 `role: "tool"`
消息注入结果。我们的子 agent 不用那个协议。改为让模型在普通消息内容
里发一个 JSON object：`{"action": ..., ...}`。三个原因：

1. **可移植性。** JSON-action 协议对任何遵循指令的模型都管用——包括
   没有 function-calling 支持的本地 Ollama、未来的后端、降级模式
   （比如模型返回纯文本 → 我们当成 final_answer）。
2. **三种 action，不只两种。** 我们加了 `report_graph_error` 让子
   agent 把图问题冒给父 agent。原生 `tool_calls` 需要为这个信号
   发明一个假工具；JSON-action 让它跟 `use_tool` 和 `final_answer`
   自然并列。
3. **可检查性。** 每条 assistant 消息都是一个可在 transcript 里 grep
   的 JSON 字符串。原生 tool_calls 协议会散在多个结构化字段里。

权衡：我们错过了后端侧的优化，比如并行工具调用和工具结果缓存。
对子 agent——那个便宜可派生的内层循环——这反而是对的选择。
主 agent（编排工作发生的地方）**确实**用原生 `tool_calls`；只有子
agent 不用。

### 为什么 loop 是单层的（不嵌套 GraphLoop）

每个子 agent 是一次性工具循环，不是一个迷你的 GraphLoop。我们考虑
过嵌套（每个子 agent 跑自己的 GRAPH ↔ TASK ↔ REVIEW 处理本地切片）
但暂时否决：

- **成本。** 嵌套循环会乘以 token 消耗。N 个子 agent 各自跑 5 轮
  图循环 = 每 Task 阶段 5N 轮图。
- **协作。** 子 agent 会有父 agent 必须合并的并行图，把 Cognition
  警告的"决策变得分散"那个失败模式又请回来了。
- **边际收益递减。** 从经验看（Demo A 的跑），单次子 agent 配
  富上下文（distance 0 的全 L0+L1+L2）+ 工具循环足以应付多数任务。

嵌套 GraphLoop 是 Phase 5+ 的备选——当子任务本身长到值得有自己的
纪律时。

### 为什么 graph error 上报时 `success=false`

子 agent 发 `report_graph_error` 时，它的 `SubAgentResult.success`
是 `false`。发现是有价值的，但子任务没完成。dispatcher 的
`all_succeeded` 标 false；循环冒 `GraphInvalid` 而非去 Review。

我们选这个（而不是 `success=true 同时 graph_errors 非空`）因为：

- 语义诚实：子任务没产出分配的结果；`output` 字段不可信
- `all_succeeded` 变成调用方检查的一个布尔；下游不需要同时看 success
  和 graph_errors 两个字段

### 子 agent 工具访问（v1 + v1.1）

子 agent 在两道护栏下跑 bash 工具：

1. **`DangerousCommandDeny`**（默认）——放行所有已注册工具，但
   拦下 `command` 字段匹配 20 条高危模式中任何一条的 bash 命令
   （`rm -rf /`、`mkfs`、`kubectl delete`、`terraform destroy`、
   `git push --force`、pipe-to-shell、磁盘重定向等）
2. **`ScopeGuard`**（按 task 自动派生）——限制 bash 写操作的目标
   在 task 的 `involved_nodes`（或它们的 distance-1 邻居）能到达
   的路径里。读默认无限制——"探索世界"是模型合法的行为。Dispatcher
   （不是调用方、不是模型）在 task 启动时从图构造这个 guard

**v1.1**：`ScopeGuard` 还把 12 个常见 build 工具识别为"隐式 cwd 写"：
裸的 `cargo build` 或 `npm install` 即使没有显式路径参数也被允许，
因为 guard 信任这些工具会写到 cwd 下的子目录（`target/`、
`node_modules/` 等），而 agent 的 cwd 在 scope 里。**确实**显式指定
出 scope 路径的命令（如 `cargo build --target-dir /elsewhere`）仍
走标准 scope 检查。verb 列表运行时按 `ScopeGuard` 实例可配——见
[`src/tools/scope_guard.rs`](src/tools/scope_guard.rs) 和设计 spec
[`docs/superpowers/specs/2026-06-03-implicit-cwd-write-verbs-design.md`](docs/superpowers/specs/2026-06-03-implicit-cwd-write-verbs-design.md)。

---

## 6. 修复架构

### 为什么逐个修，从不批量

三个力推批量修（一次修完所有问题，然后重验证）：

- 吞吐：更少 model 调用
- 原子性：调用方看到"图修了"而不是"9 个 patch 落地"
- API 更简单：一个 `repair_all(errors) → patch` 而不是
  `for err { repair(err) → apply }`

我们仍然选逐个修：

- **批量丢信号精度。** 每个错误都是图和现实之间的一个具体矛盾。
  塞进同一个 prompt 让模型同时权衡——意味着它可能只修最显眼的，漏
  掉别的。
- **批量产生回归风险。** 一个模型为了修 3 个错误重写半个图，偶尔
  会不小心引入第 4 个。逐 issue patch 是外科手术：模型只动你给它的
  scope。
- **用时间换空间在本地模型下很便宜。** 当推理成本低时，付 3 个
  patch × N token 的成本低于 1 个 patch × 3N token 的成本，后者还
  更糟。

设计原则 #2（time-for-space）是这个一般化：宁可多修小处，不修一
次大处。

### 一套机制,两种触发:`LocalRepairer` 与子 agent 重派

`LocalRepairer` 是"逐 issue、scoped"patch 的标准实现。今天它由
`Verifier` 在 Graph 阶段触发(高严重度 issue)。同一套机制 — 同样的
patch 形状、同样的 scope 规则、同样的"一次一个 issue"纪律 — 也适用
于第二个触发源,整个架构默认这件事:**Task 阶段子 agent 报告失败**。
子 agent 返回 `success=false` 加 `report_graph_error` 时,orchestrator
的响应是一个 `GraphPatch`,scope 锁在那一个子节点的 spec(对应节点的
`Node.metadata["spec"]` 或等价物),apply 之后循环**只重派那一个子
agent** — 不是整个 task graph。按 §1a 的"核心思想":子 agent 失败
和 verifier 发现,在形状上没有区别 — 都产生一个 per-node
`GraphPatch`,触发一个组件的重跑。

### Scope 强制

`LocalRepairer::validate_scope` 拒绝任何动到 issue scope 之外节点
（或它的 1-hop 邻居、或 patch 自己加的节点）的 `GraphPatch`。错误
消息包含哪个节点越界。这是机械的，不是启发式的：可以告诉模型
"待在 scope 内"，但执行是 runtime 的事。

### 为什么调用方驱动自动修复，不是 GraphLoop

`GraphLoop` 把 `GraphInvalid` 浮给调用方；调用方对每个错误调
`LocalRepairer::repair_from_error`，应用 patch，调
`resume_with_repaired_graph(repaired)`。Demo A 这部分 30 行。

我们考虑过把自动修复做进 GraphLoop 内部（配置上的
`auto_repair_budget` 字段）。三个反对理由：

- **调用方策略各异。** CLI 想打印进度；CI 想快速失败；Web UI 想
  问用户。内部自动修复强行一种策略。
- **可观测性。** 外部自动修复在调用方日志里以正确的抽象级别出现；
  内部的话会埋进 GraphLoop 的 tracing 里。
- **测试面。** 被动的 `GraphLoop` 更易测（设初态 → step → 断言返
  回态）——比带可配内部修复预算的版本简单。

纪律是：循环的 API 是"刚发生什么，你想怎么办？"。调用方决定是
否自动重试。

---

## 7. 验证分层

### `Verifier` 里的三层

```
Layer 1 — 结构化（Graph::find_inconsistencies）：
    悬空边、孤儿节点、要求无环关系中的环、重复边、无效 confidence
    确定性。无 model 调用。总是跑。

Layer 2 — model 自检：
    给定（图, 任务, 对话），图是否够支撑任务？缺什么？夸大了什么？
    错了什么？
    一次 model 调用。可跳过（`Verifier::structural_only()`）。
    v2.6 后：声明 `graph_self_check_verdict` tool schema；优先
    `tool_calls`，回退文本 JSON。

Layer 3 — L1 采样：
    抽 N 个有非空 L1 的节点。对每个，通过 SourceLoader 取 L2，
    问 model：L1 跟 L2 还对得上吗？
    N 次 model 调用。需要已配置的 loader。
    v2.6 后：每个节点声明 `l1_check_verdict` tool schema。
```

各层由"什么可用"闸控，不由 confidence：结构化检查总是跑；对比
L2 的检查需要 loader；model 自检需要 model。`Verifier::structural_only()`
供单元测试绕过 model。v2.6 之后，模型面（Layer 2 和 3）优先用
OpenAI `tool_calls`，保留文本 JSON 路径作为兜底——见 近期变更和
`CLAUDE.md` 的迁移故事。

### 为什么是三层，不是一层（"问 model"）？

让 model 在一个 prompt 里判断所有事——"这个图够好吗？"——有两个
失败模式：

- Model 说"看着不错"即使图里有悬空边（它懒得结构化检查）
- Model 说"fail"加上模糊理由，runtime 拿不到精确的 issue 去修

带类型化 issue 的结构化层解决这两个。每个 `VerifyIssue` 有
`scope`（哪些节点）、`severity`、`source`（哪层发现的）。`LocalRepairer`
用这些去定修复 patch 的 scope。

---

## 8. 并发与取消

### Dispatcher 用 `tokio::sync::Semaphore`

在 `SubAgentPool::run_batch` 里，每个子任务被 spawn 成一个 tokio
future；`Semaphore` 限定 in-flight 数到 `max_concurrent`。这给
我们：

- 真并行执行（由 `pool_actually_runs_batch_concurrently` 验证）
- 有界并发（由 `pool_respects_max_concurrent_limit` 验证）
- 顺序保留（结果按 batch 顺序返回，与 spawn 完成顺序无关——按
  顺序收集 `JoinHandle` 然后按顺序 await）

### 为什么不在 batch 之间取消

如果 batch 1 的子 agent A 上报 `report_graph_error`，batch 2 还没
启动——我们自然不跑（循环在发下一 batch 之前返回 `GraphInvalid`）。
但 batch 1 内部，其他 in-flight 的子 agent **不**被取消；我们等它
们完成、收集结果。

考虑过"第一个错误就取消 batch"但否决了：

- tokio 里取消要么靠 `CancellationToken` 走遍每个 future，要么
  abort `JoinHandle`（无法干净杀掉底层 model 调用的 HTTP 请求）
- 让兄弟跑完通常产出**更有用**的结果——A 的 graph error 加上 B
  的 task result 比只有 A 的错误信息量更大
- Phase 4 v1 里跑一个已 spawn 的子 agent 的成本被 `max_steps` ×
  per-call latency bound 住了，"浪费"的工作量是有限的

### Join 错误 vs 子 agent 错误

`SubAgent::execute` 内部捕获 model 错误并返回 `SubAgentResult::failure(...)`。
Dispatcher 只把 tokio `JoinError`（spawned future 里的 panic）当成
致命 `HarnessError`。这个区分有意义：超时、被 policy 拒绝、网
络抖动的子 agent 不应该污染 batch——它的结果被捕获，循环继续。
只有程序 bug 才应该杀掉 batch。

---

## 9. 对话与上下文管理

### 为什么图 snapshot 每次 model 调用都重新注入

`Conversation::to_request` 里，在 system prompt 之后总会注入第二条
system 消息：

```
Current relationship-graph snapshot (authoritative — your beliefs about
the graph should match this):
{render of current graph}
```

每轮都花 prompt token，但消除了"模型脑子里的陈旧图"那个失败模式。
模型从不需要记中间的图状态；每轮都看当前状态。

替代方案——只发图 delta——假设模型能精确跟踪运行中状态，这是那种
长对话里微妙失败的隐式状态跟踪。

### 为什么 snapshot 用纯文本而不是 JSON

prompt 里的图 snapshot 长这样：

```
graph version=3 status=Draft nodes=5 edges=4 l1_entries=5
nodes (L0 + L1 oneline):
  - id=auth.rs kind=File summary="handles JWT" L1="signs and verifies tokens" (c=0.85)
  - id=db.rs kind=File summary="storage" L1=(not yet enriched)
  ...
edges:
  [0] auth.rs -[Imports c=0.90]-> db.rs  evidence="use crate::db"
```

纯文本，不是 JSON。理由：模型推理时，几个排版好的纯文本行比嵌套
JSON 强，token 也更便宜。JSON 形态的字段会触发模式匹配（"这是
数据，透传过去"），散文形态的状态能引发更深的推理。

### `Conversation` 不拥有图

`Conversation` 装消息历史。图住在 `GraphLoop` 上。每次 `to_request`
调用把当前图 snapshot 作为字符串参数传入——这意味着同一个
Conversation 可以穿过多次图变更而不陈旧。边界让每种类型的职责
保持窄。

---

## 10. 工具层

### `Tool` trait + `Policy` 分离

```
Tool trait:    这个工具能做什么？schema 是什么？输入怎么分类？
Policy trait:  在这个上下文里，这个特定输入允许跑吗？
ToolContext:   在哪？（cwd）多少输出算太多？哪个 policy 把这次调用？
ToolRegistry:  名字 → 工具。invoke() 是单一执行入口。
```

一个 `Tool` 声明按输入的分类（`is_read_only`、`is_destructive`、
`is_concurrency_safe`）。一个 `Policy` 咨询这些分类 + 工具名 +
输入，决定 `Allow / Deny / AskUser`。每次调用走
`ToolRegistry::invoke` → policy 检查 → `Tool::call`——policy 门
是单一咽喉点。

默认 `SubAgent` 策略是 `DangerousCommandDeny`，不是 `AllowAll`：
放行所有已注册工具，但拦下 `command` 字段匹配高危模式
（`rm -rf /`、`mkfs`、`git push --force`、pipe-to-shell 等）的 bash
命令。一个互补的
[`ScopeGuard`](src/tools/scope_guard.rs) 按子任务从 task 的
`involved_nodes` 自动派生，把 bash 写操作限制在这些节点（或它们
的 distance-1 邻居）下的路径。读默认无限制——探索世界是模型
合法的行为。模型可自由选任何已注册工具；这两道护栏是它和 shell
之间仅有的东西。

为什么把门放在 `Tool::call` 之外？因为那意味着每个工具都得实现
policy 逻辑，policy 会跟工具实现耦合。分开之后：

- 新工具自动获得 policy gating
- 自定义 policy（白名单、时段、角色、纯审计）可插在 `ToolContext`
  层
- `DangerousCommandDeny`（默认）/ `ReadOnly` / `AllowAll` /
  `AllowList` / 自定义 `Policy` impl 覆盖了常见场景

**v1.1 扩展（IMPLICIT_CWD_WRITE_VERBS）：** `ScopeGuard` 还把 12 个
常见 build 工具识别为"隐式 cwd 写"（`cargo`、`rustc`、`go`、
`node`、`npm`、`yarn`、`pnpm`、`python`、`python3`、`pip`、
`pip3`、`make`）。当这些工具以无显式路径的形式被调用时，scope
检查被跳过（假设工具写到 cwd 下的子目录如 `target/` 或
`node_modules/`）。bash 算法从 7 步扩到 10 步：compound 操作符
检查现在跑在"unrecognized"检查**之前**，所以任何带 compound 操作的
命令先被结构性理由拒绝；新加"empty + implicit_cwd → Ok"分支让裸
`cargo build` 通过。Verb 列表运行时可配：
[`ScopeGuard::with_implicit_cwd_verb`](src/tools/scope_guard.rs) /
`without_implicit_cwd_verb` / `reset_implicit_cwd_verbs`。详见 spec
[`docs/superpowers/specs/2026-06-03-implicit-cwd-write-verbs-design.md`](docs/superpowers/specs/2026-06-03-implicit-cwd-write-verbs-design.md)
以及 `README.md` §Build tool caveats 披露的限制。

### 为什么是按输入分类，不是按工具

`BashTool` 配一个 `is_read_only` 常量就是说谎：`bash` 本身不是只
读的；决定它的是命令。所以 `BashTool::is_read_only(&self, input)`
看实际命令：

- 首 token 白名单（`ls`、`cat`、`grep`...）
- 多词前缀（`git log`、`cargo check`、`rustc --version`...）
- 任何带重定向、管道、`$()`、`;`、`&&` 的都判出局

这是从 Claude Code 的 `isReadOnly(input)` 模式借鉴的。runtime 信
按调用分类，因为区分性信息在输入里，不在工具的 identity 里。

### 尾部截断，不是头部截断

`truncate_tail(text, max_chars)` 保留最后 `max_chars` 个字符，前面
加 `[…N chars truncated…]` 标记。理由：命令/测试/日志输出的惯例是
有意思的东西在结尾（错误消息、退出状态、摘要）。头部截断会常常
藏住失败原因。这是 Claude Code 的 `EndTruncatingAccumulator` 模式
移植过来的。

---

## 11. 配置与模型分层

### 为什么是两层模型

单次 agent 跑会碰 ~15 次 model 调用。只用一层，要么所有地方付深
模型成本（贵），要么所有地方接受快模型质量（图分解变得潦草）。
两层让 harness 把调用路由到对的模型：

| 组件 | 层 | 理由 |
|---|---|---|
| `GraphProposer` | fast | 每次跑多次；每次短；质量只要"足够好的 JSON" |
| `Verifier` | fast | 频繁重检；确定性层承担主要工作 |
| `SubAgent` | fast | 每个 task 一个；单次工具循环；模式匹配占主导 |
| `L1Enricher` | deep | 每个节点一个；产出是下游依赖的结构化语义 |
| `LocalRepairer` | deep | patch 必须一次成；坏 patch 成本 > 深调成本 |
| `Decomposer` | deep | 任务分解高杠杆；一次坏拆分浪费 N 个子 agent |
| `Reviewer` | deep | 每次跑一次判断；质量比吞吐重要 |

配 `MODEL_NAME_FAST=deepseek-v4-flash MODEL_NAME_DEEP=deepseek-v4-pro`，
典型 Demo A 跑 ~$0.03 而不是 ~$0.10。分层也减短墙钟：flash 1-2 秒
vs pro 同输入 5-15 秒。

### 为什么是 env 驱动，不是配置文件

`.env` 是 shell 原生：每个开发机、每个 CI runner、每个容器、每个
IDE 都懂环境变量。自定义配置格式要 loader、要 schema、要迁移、
要回答"放哪"。`dotenvy` 进程启动时读一次 `.env`，回退到现有 env——
零仪式。

对编程式调用方，`ModelConfig::new(...)` 完全跳过 env、直接传值。

---

## 12. 设计原则展开

十二条原则驱动每个组件。简版（带 TL;DR 标题）在 `README.md`；
下面是完整推理。前六条早于 v2.6；后六条是在 db2d993d 之后的设计
复盘和 tool_calls 迁移中浮现的。

### 1. Model-agnostic（模型无关）

永不把模型名硬编码进源码。任何命名都通过读 env 的 `ModelConfig`
走。理由：实证工作显示 harness 增益在模型之间可迁移。把 harness
耦合到具体模型是在浪费那种迁移。

代码上：任何你想写 `"gpt-4o"` 或 `"claude-opus"` 的地方，改写
`cfg.fast_model()` 或 `cfg.deep_model()`。需要特定模型行为的测
试用 `MockModel` trait impl。

Reasoning-only 模型（DeepSeek-v3、MiniMax M3）是一等公民：每一
层读取模型文本响应时都走 `ModelResponse::text_or_reasoning()`，
它在 `content` 非空时优先用它，否则用 `reasoning_content`。
这是 v2.6 对 db2d993d 生产故障的修复。

### 2. 图是计划、是调度、是审计日志

三件不同的事，同一个数据结构。**计划** — 主 agent 把图当作工作
记忆来编辑。**调度** — `DagScheduler` 在 `DependsOn` 边上跑 Kahn
算法，产出 wave-aligned 批；dispatcher 只是执行它们。**审计日志**
— `CheckpointStore` 在每次有意义的变更后快照 `(round, phase,
graph, transcript)`，配 `branches` 映射支持 fork。机制分别见
§2、§8、§13。

### 3. 确定性优先于 LLM 评判（Defense in depth）

系统里有很多"信任模型"的决策。**没有一个是硬门（hard gate）。**
每个都被至少检查两次——一次是确定性机制，一次是 LLM-as-judge 顾问：

- "图结构一致" — `Graph::find_inconsistencies`（确定性）。无 LLM
  顾问（太简单了）。
- "子 agent 工作正确" — `CheckContract` 被**检查两次**：子 agent
  自己一次，dispatcher 再查一次。
- "代码能编译" — `PostExecutionValidator` 跑 `cargo check` / `tsc`
  并对 stderr 做 graph vs task 错误模式匹配。
- "L1 与 L2 一致" — 子串比较 + drift 严重度（确定性）+ `l1_check_verdict`
  （顾问式；从不单方面判失败）。
- "最终结果可接受" — 确定性 reviewer（图一致性、子 agent 成功、
  verify-phase 状态）+ `judge_verdict`（顾问式；`root_cause` 路由
  到 repair）。

**不可靠的模型不能让结构上正确的图崩掉。** 这是系统最重要的
安全属性。任何新的"信任模型"决策必须配套一个确定性的第二线
检查，否则不能上。

### 4. 边界处窄协议，内部宽协议

主 agent 用宽的 OpenAI `tool_calls`（6 种 step 类型，完整 GraphPatch
schema）。子 agent 用窄的自定义 JSON-action。Skill 编译用更窄的
（`Task + DependsOn` only）。完整表格见 §5。看到这种不一致，第一个
冲动是**统一**——别这么做。每次收窄都是 defense-in-depth 决策：
边界处协议越窄，模型在该层失控时爆炸半径越小。如果将来有贡献者
提议统一协议，问题只有一个：**我们会丢掉什么安全保证？**

### 5. 三个正交的记忆层

| 层 | 存储 | 生命周期 |
|---|---|---|
| 结构（graph）| 内存中的 `Graph` + checkpoint 到磁盘 | 一次 run |
| 提示词（conversation）| 内存中的 `Conversation` | 一次 run |
| 编译后（skills）| `LocalSkillStorage`（文件系统）| 永久 |

这三个**正交**：skill 不漏到 graph，graph 不漏到 conversation，
conversation 不漏到 skill。每一个是独立的数据结构，有自己的写入
路径。新的"记忆"功能应选一个层和一条写入路径；抵制"哪里都放一份"
的诱惑。集成点是 Task 阶段的 `try_match_and_compile_skill`。

### 6. Skill 是结构化记忆，不是提示词记忆

当一次 run 成功到达 `ready_for_verify`，`skills::capture::capture_skill()`
抽取 `propose_patch` 序列作为编译后的任务 DAG，存到本地。下一次有
token-Jaccard ≥ 0.25 匹配的任务，**完全跳过 decomposer**，直接用
编译后的 skill 图。这是结构化记忆：skill 是图拓扑，不是提示词
片段。成功的 run 会复利——agent 在已经做过的事上越来越快，速度
提升也建立在那驱动一切的同一种 artifact（`Graph`）上。

编译器（`skills::compiler::compile_skill_to_task_graph`）是**纯函数**：
同输入同输出。无 I/O、无 model 调用、无随机性。输出直接喂给
`DagScheduler`；`dag_is_schedulable` 测试断言这个契约成立。
id 前缀 `skill:<slug>:<node_id>` 防止与宿主 run 的任务图冲突，
metadata 带 `skill_slug` / `skill_trigger` / `skill_node_id` 记录
完整 provenance。

### 7. 子 agent 是被约束的，不是被信任的

子 agent 跑的时候有三层独立约束，**全部**用代码强制：

1. **`max_steps`**（默认 8）—— 每个子 agent 的模型调用次数硬上限。
2. **`ScopeGuard`** —— 每个 `use_tool` action 在调用**前**对照
   允许路径策略做检查。一个被派去"修 `auth.rs`"的子 agent 不能写
   `/var/log` 或 `~/.ssh/`。
3. **`CheckContract`** —— 子 agent 的 `final_answer` 对照一个确定性
   谓词做验证（`KnowHow` / `Exploratory` / `MustEdit`）。检查
   **跑两遍**——子 agent 自己一遍，dispatcher 再查一遍。

再加一个 `report_graph_error` action，让子 agent 在发现图本身
有问题时**把 `GraphError` 冒泡**到主循环。

### 8. Time-for-space（拿错误换正确）

宁可多修小处，不修一次大处。每个执行中抓到的错误都是精度信号
——别把它们打包。

代码上：`LocalRepairer::repair_from_error` 收**一个**错误，返
回**一个**patch。Verifier 在每个 patch 后重跑。没有 `repair_all`。

这个原则来自用户早期对"打包错误然后一起修"建议的反弹。论点是：
打包丢失了每个错误编码的具体矛盾信号。

### 9. Local graph repair, never bulk（局部图修复，从不批量）

verifier 找到问题时，逐个用子图 scope 内的 patch 修。全图重建是
显式 opt-in，不是错误路径。

代码上：`LocalRepairer::validate_scope` 拒绝动到 issue scope 外
节点的 patch。没有"从零重建图"的 API——那是不同层级的不同操作。
这让完整的执行历史可 checkpoint：每个 repair 都是一步小、可检
查、可回退的操作。

### 10. Universality lives in the model, structure lives in the graph
（通用性在模型，结构在图）

harness 在领域间是通用的；领域特定判断委托给模型。别把领域
枚举塞进共享类型。

代码上：`NodeKind` 是 `File / Function / Class / Module / Config /
Task / Other(String)`。具名变体是通用抽象；领域特定种类进
`Other("database")` 加 metadata。`RelationType` 同理。

即使在实现新领域时也省事——你不用改共享类型就能引入
`NodeKind::TerraformResource`。塞 `Other("terraform_resource")`
加 metadata 即可；harness 通用地处理它。

### 11. Reviewer needs deterministic backstops（Reviewer 需要确定性 backstops）

LLM-as-judge 单干不可靠。在信任 model 裁定之前叠多层确定性
检查。

代码上：`Reviewer::review` 跑确定性检查（图一致性、子 agent
成功、last_verification）然后才调 LLM judge。裁定是
`passed = det_passed && judge_passed`（两者都得过），且 deterministic
fail 覆盖 judge pass（由测试 `deterministic_fail_overrides_judge_pass`
验证）。

这是上面 #3 的具体实例化——#3 是系统范围版，本条特指 Reviewer。

### 12. Scanners are seeds, not the product（Scanner 是种子，不是产品）

code/data/infra scanner 产出低置信度（≤ 0.6）的起始图。模型才是
真正的图构建器。别在 scanner 巧妙性上过度投入。

代码上：`CodeScanner` 发边的 `confidence: 0.6`（字面常量）。
Phase 2.5 的 L1Enricher 才是产出高置信度语义的 model 驱动路径。
scanner 存在是为了让模型在 code-domain 跑时有**东西**起步；对非
code 域根本就没有 scanner（`NullSourceLoader`）。

---

## 13. 权衡与已知限制

### Token 成本 vs 准确性

每个提高准确性的设计选择（每轮图 snapshot、每个 patch 后重验证、
确定性 Reviewer backstops、多层）都花 token。典型 Demo A 跑用
40-70K token。我们优先准确性，因为：

- Token 便宜且越来越便宜（deepseek-v4-flash < $0.30/百万）
- 错的工作贵——抓到的错工作会叠加；漏掉的错工作进生产
- harness 是为非平凡任务设计的，在那里替代"花 token"的是"人审时间"

如果你的任务小到 token 主导成本，配
`Verifier::structural_only()`（跳 model 自检）、
`Reviewer::deterministic_only()`（跳 LLM judge）、跳过 validator。
harness 仍能跑；只是更依赖便宜的层。

### `max_tokens` 和推理模型

推理模型（DeepSeek-v4-pro、GPT-o1、带 extended thinking 的 Claude）
在发出可见 JSON action 前会烧 5-20K token 的内部推理。Output 封
顶 8K → JSON 被截 → "unterminated JSON object" 错。我们把
`GraphProposer.max_tokens` 默认提到 32K，但根本问题是推理模型改
变了 max_tokens 和可用输出之间的关系。

harness 里的缓解：
- `max_tokens` 默认提到 32K（Proposer）/ 8K（Decomposer）等
- 分层把高频调用路由到 `flash`（非推理），没这问题
- JSON 解析器容忍纯文本响应（子 agent 当成 final_answer；其他地方
  作为 `ProposerStep` parse 错误浮出来）

更深的问题——也是 v2.6 tool_calls 迁移重要的原因——是对于
**reasoning-only 模型**（DeepSeek-v3、MiniMax M3），最终 JSON
经常在 `reasoning_content` 里而不是 `content`。纯文本 JSON 解析
在这些模型上必死。`ModelResponse::text_or_reasoning()` 辅助方法
是核心修复；`tool_calls` 迁移是结构性修复（API 强制结构化输出，
不管模型选在哪 emit）。

### 边界处协议收窄（v2.6 的权衡）

v2.6 全员 tool_calls 迁移**没有**统一主 agent 和 sub-agent 的
协议。三层、三种协议、三种不同宽度——见 §5 的完整表格。

代价是真实的：

- **三个解析器，三套测试。** 文本兜底路径在每一层都保留（没有
  function-calling 支持的模型仍要能跑），所以迁移是加法，不是
  替换。代码库同时携带 `parse_*_from_tool_calls` 和
  `parse_*_from_text` 两条路径。
- **协议越窄，边界处的契约验证越重要。** 子 agent 尤其窄协议
  （JSON-action，三种 action 类型）；模型如果 emit 这三种之外
  的 action，会拿到一个结构化的拒绝 + retry 提示。
- **实时 tool_call 流式是不对称的。** 非流式 `complete()` 路径
  对每个 tool_call 发一个组装好的 `StreamToolCall`；SSE
  `complete_stream()` 路径发按片段的 `StreamDelta::ToolCallArgument`
  事件，前端要按 `index` 组装。前端 `RunView.vue` 现在只消费组装
  好的形态；按片段的路径是给未来 SSE UX 留的。

收益是代价换来的：在子 agent 层失控的模型不能外泄它的窄契约
（没法生造新的 "action" 而不被 parser 抓到）；在 skill 层失控
的模型不能夹带 `GraphPatch`（编译器只收 `Task + DependsOn`）。
协议越窄，爆炸半径越小。**别统一。**

### 子 agent 里不嵌套 GraphLoop（还没有）

子 agent 是单次工具循环。它们能通过 `bash` 读源，但没法在私有
子图上跑自己的发现 → 修复 → 执行循环。对子任务本身就不平凡的
情况（比如"设计并实现一个新模块"），这表现为子 agent 收敛失败。

Phase 5+ 范围项：嵌套 GraphLoop，共享父的 L0+L1，私有 L2 访问。

### 流式输出

每次模型调用都经过 `ModelWithEvents`（[`src/model/streaming.rs`](src/model/streaming.rs)），
一个透明的 `Arc<dyn Model>` 包装器。当 `complete()` 被调用时：

1. 包装器创建一个 mpsc channel，调用 `inner.complete_stream()`。
2. `OpenAICompatModel::complete_stream()` 打开 SSE 连接
   （`stream: true`），逐行解析 `data:` 内容，每个 token 发送
   `StreamDelta::Delta { content, reasoning_content }`，结束时发送
   `StreamDelta::Done { finish_reason, usage }`。
3. `ModelWithEvents` 中的转发任务读取这些 delta，向 session 的
   broadcast channel 发送 `RunEvent::StreamChunk` / `RunEvent::StreamEnd`。
4. WebSocket 处理器将这些事件转发给所有连接的客户端。
5. 前端在可折叠块中显示 `thinking` 内容，并将流式文本追加到当前
   `assistant_streaming` 消息。`stream_end` 时角色锁定为 `assistant`。

没有实现 `complete_stream()` 的模型走 trait 的默认实现：发一个含
完整内容的 `Delta`，然后 `Done`。流式传输对所有调用方透明——agent
循环中的任何组件都不知道自己在跟流式还是非流式模型对话。

### 持久化

`RunPersistence`（[`src/web/persistence.rs`](src/web/persistence.rs)）
将每个 run 存入 `data/runs/<id>/`：

```
data/runs/<run_id>/
  run.json                         -- RunMetadata（任务、状态、耗时）
  checkpoints/0000.json ... N.json -- 每个 checkpoint 一个 JSON 文件
  branches.json                    -- HashMap<checkpoint_index → [child_run_ids]
```

- `run.json` 在创建 run 时写入，完成/错误/取消时更新。服务启动时从
  磁盘加载所有 run 元数据，`GET /api/runs` 返回跨重启存活的已完成 runs。
- 每次 checkpoint push 触发一次文件写入。内存中的 `CheckpointStore`
  是主存储；磁盘写入是尽力而为（错误只 trace，不抛出）。
- 分支记录按 run 持久化，随 run 重新加载。

磁盘格式使用 `serde_json`，类型与内存表示相同（`RunMetadata`、
`Checkpoint`、`Graph`）。暂无迁移层——格式变更通过 `#[serde(default)]`
保持向后兼容。

### v1.1 build 工具护栏：三个已知限制

`IMPLICIT_CWD_WRITE_VERBS` 规则（让裸 `cargo build`、`npm install`
等无显式路径也能跑）有意识地把更细粒度的控制换成模型的易用性。
三个已知漏洞，每个都有文档化的 mitigation：

1. **系统级 install 命令是被允许的。** `cargo install foo`、
   `pip install foo`、`npm install -g foo` 落入同样的规则即使它
   们写到 `~/.cargo/`、site-packages 或全局 node_modules。Guard
   不解析工具行为就检测不到。**Mitigation：** 调
   `ScopeGuard::without_implicit_cwd_verb("cargo")`（或 `pip` /
   `npm`）在更严的环境下禁用该 verb。

2. **build 工具检测只看第一个 token。** 一个 shell 别名叫
   `cargo` 写 `/etc/` 会通过 verb 检查。`DangerousCommandDeny` 抓
   破坏性 payload；scope 检查抓显式出 scope 的路径。两者都不
   抓巧妙别名。**Mitigation：** 相应地信任模型；如果环境敌对，
   叠额外的 policy。

3. **`cargo run`、`cargo test`、`cargo bench` 是被允许的。** 它们
   执行任意代码。黑名单抓不到。**Mitigation：**
   `ScopeGuard::without_implicit_cwd_verb("cargo")` 禁用所有 cargo
   调用，或挂一个按子串抓 `cargo test` / `cargo run` 的自定义
   `Policy`。

这些是诚实的 v1.1 限制；堵这些洞是 v1.2 范围（per-subcommand 排
除列表、精确的"这个写到 `<subdir>`"检测）。见 `README.md` §Build
tool caveats 的用户面披露。

### 不正式的 tool-call 协议回退

如果用户真的想要 OpenAI 原生 `tool_calls`（为了并行工具执行、
服务端缓存等），需要：

- 扩展 `Message` 加 `tool_call_id` 和 `tool_calls` 字段
- 更新 `OpenAICompatModel` 序列化
- 重写 `SubAgent::execute` 用原生协议
- 加 tool definition 序列化器

直接但非平凡。JSON-action 协议覆盖了当下所需。

---

## 14. 考虑过又否决的

### Reactive vs proactive 验证

考虑过：只在出问题时跑 Verifier（便宜）。选了主动：每次 proposer
说 `ReadyForVerify` 都跑。理由：早期抓问题比三阶段后从失败回溯
便宜。

### 单一 Reviewer / Verifier / Validator 统一类

考虑过：一个 `Acceptor` trait，三个实现。选了三个独立类。理由：
每个签名不同（`Verifier` 拿 `Option<&Conversation>`，`Reviewer` 拿
`DispatchOutcome`，`PostExecutionValidator` 返回裁定 enum 而不是
`VerificationResult`）。统一它们会迫使调用方为不适用的字段构造
假值。

### 图存为 RDF 三元组或 SQLite

考虑过：存进真 DB 取查询力。选了内存 `HashMap<NodeId, Node>` +
`Vec<Edge>`。理由：图是单次跑的工作内存，不是持久态。磁盘格式
的代价（序列化、查询层、schema 迁移）远超我们对访问模式（BFS、
局部子图、全扫描）的收益。

### 基于调用复杂度自动路由模型层

考虑过：harness 根据 prompt 大小或任务复杂度自动选 fast vs deep。
选了显式按组件分层。理由：分层是深思熟虑的策略决策（质量 vs
吞吐权衡），不是输入的函数。自动路由会藏掉策略，让成本不可预测。

### 通用 `Result<T, GraphError>` 包打天下

考虑过：用区分图错误 / 网络错误 / JSON 错误 / policy 错误的领域
特定错误类型替换 `HarnessError`。选了扁平的 `HarnessError` enum
配 String 载荷。理由：在乎特定错误类型的调用点已经按 variant
做模式匹配（model / domain / scanner / io / serde / context /
scheduler / graph）。再加结构成本超过收益。

---

## 15. 参考

harness 的智力资本来自几个公开来源：

- **Anthropic, "Building Effective Agents"** (anthropic.com/research/building-effective-agents)
  —— orchestrator-workers 和 evaluator-optimizer 模式，"agent vs.
  workflow"框架，停止条件的纪律
- **SWE-agent 论文** (arXiv:2405.15793)——经验证据：Agent-Computer
  Interface 设计是一阶性能杠杆
- **Reflexion 论文** (arXiv:2303.11366)——语言强化 / 情节记忆。我们
  的 `GraphError` → 修复流是它的一个具体化变体，反思变成图编辑
- **LangGraph** (langchain-ai/langgraph)——图即控制流；我们用图即
  世界模型。不同层级，但图形态的 agent 状态这个先例值得引
- **Cognition, "Don't Build Multi-Agents"** (cognition.ai/blog/dont-build-multi-agents)
  ——警告并行子 agent 容易漂移开。我们的防御是共享图作为基底。
  这够不够是 Phase 5+ 的问题
- **Claude Code 源码**（从公开 npm 包的 sourcemap 反推）——作为
  工具架构、按输入分类、尾部截断、`isReadOnly` 模式的参考。没
  有代码逐字复制；模式用 Rust 重写

对更深的"binding constraint"框架（长任务上 harness > model），其
底层论证散布在上述来源里，但没归到一个论文。我们项目赌的是
这个框架是对的。
