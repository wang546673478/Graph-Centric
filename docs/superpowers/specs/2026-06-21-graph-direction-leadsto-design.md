# 图方向翻转 + 关系类型多元化(LeadsTo)设计

日期:2026-06-21
状态:设计已确认,待写实现计划
这是「图方向 + 目标澄清」改造的**第二期**(第一期目标澄清已完成,见 [[graph-direction-and-goal-clarification]])。

## Context

当前 seed 把开始↔目标建成 `D --DependsOn--> A`(source=D, target=A,"目标依赖开始"),节点 id 是 `A`/`D`。这有两个问题:

1. **方向和语义都反了**。用户的正确模型:`start → deliverable` 是**流向**(从哪里开始、最终交付什么),不是"目标依赖开始"。中间节点是**过程**(开始之后怎么一步步到交付)。
2. **关系类型单一**。现在整图统一用 `DependsOn`(依赖语义)串所有节点(主链、decomposer 子任务、cascade_expand 子图)。但真实任务里,关系语义随任务类型不同:写文章是纯线性流向(LeadsTo,无环);构建系统里组件间是依赖(DependsOn,无环),而流程可能需要回环(LeadsTo,允许环)。**用哪种边、要不要环,应由模型按任务判断**——契合本项目 "universality via model" 的核心思想。

`A`/`D` 命名也有问题:字母编号暗示固定的步骤数量,而中间步骤数量任意。

## 已确认的设计决策

> **一句话不变量**:唯一固定的是 `start --LeadsTo--> deliverable` 这条主轴用 `LeadsTo`。中间插入的所有步骤节点之间怎么连(LeadsTo / DependsOn / Contains、要不要环)——**全部交给模型按任务类型自主判断**,系统只提供关系图谱和约束,不预设中间结构。

1. **`start → deliverable` 主链固定用 `LeadsTo`**(流向);锚点 id 从 `A`/`D` 改为语义化的 `start` / `deliverable`。
2. **中间过程步骤的边类型交给模型判断**:模型先判断任务类型,再从关系图谱(LeadsTo/DependsOn/Contains/...)挑合适的边自己连接。流程先后→LeadsTo;真正依赖→DependsOn;包含→Contains。
3. **关系类型并存**:新增 `LeadsTo`;`DependsOn`/`Contains` 等全部保留。**彻底不再把 `DependsOn` 当作主链默认**(主链改 LeadsTo)。
4. **无环约束按关系类型分**:`DependsOn`、`Contains` 必须无环;`LeadsTo` **允许有环**(流程可回退/循环/轮询)。
5. **连通性遍历认所有结构边**:从 `start` 出发,沿任意结构边(LeadsTo + DependsOn + Contains)判断节点是否连通——模型用哪种边连中间步骤,replay 都不误判孤儿。
6. **方向语义翻转**:从旧的"所有节点沿出边指向锚点 A(A 是汇点)"翻成"从 `start` 沿出边流向 `deliverable`(start 是源点)"。
7. **无旧数据兼容负担**:目前没有正式任务/持久化的正式数据,直接一步到位,无需兼容兜底。

## 架构

### 关系类型(`src/graph/mod.rs`)

`RelationType` 枚举**新增** `LeadsTo`(放在结构/依赖之间,语义为"流向/过程")。`parse_wire`/序列化/`to_str` 各加 `LeadsTo` ↔ `"LeadsTo"`。`DependsOn`/`Contains` 等保留不变。

### seed 建边(`src/agent/graph_loop.rs`)

`auto_seed_start_goal` 及 step_graph 里的 seed 逻辑:
- 节点 id:`A` → `start`(immutable anchor),`D` → `deliverable`(goal)
- 边:`D --DependsOn--> A` → **`start --LeadsTo--> deliverable`**(source=start, target=deliverable)
- 所有引用 `"A"`/`"D"` 字面量的地方(seed 裁剪、auto_seed、单测)同步改为 `start`/`deliverable`

### 方向翻转(核心不变量,`src/agent/graph_loop.rs` + `src/agent/cascade.rs`)

旧约定:节点沿**出边**最终到达锚点 A(A 是 sink)。新约定:`start` 是 source,沿**出边**流向 deliverable。

- **`anchor_goal_connected`**:`start` 能否沿出边流到 `deliverable`(`path_exists(start, deliverable)`),取代旧的 `path_exists(D, A)`。锚点用 `immutable` 标志找(仍是 start)。
- **`replay_from_anchor`**:孤儿判定从"节点能否到达锚点"翻成"**锚点 `start` 能否到达该节点**"(`path_exists(start, node)`)。沿出边 BFS 从 start 出发,到不了的非锚点节点 = 孤儿。
- **`path_exists`**:保持通用 BFS(from→to 沿出边),调用方向调整即可;**连通性遍历认所有结构边**(LeadsTo/DependsOn/Contains 都算可走边,见下)。
- **cascade `dependency_predecessors_of`**:找某节点的"上游"(喂给它输入的)= 该节点的**入边**来源(`target==node` → `source`),取代旧的 `source==node` → `target`。上游来源认 LeadsTo + DependsOn。

### 连通性的"结构边"定义

新增一个判断:哪些 relation 算"结构边"(用于连通性/replay 遍历)= `LeadsTo`、`DependsOn`、`Contains`(即除纯元信息 RevealedBy/InvalidatedBy 外的结构关系)。`path_exists` / `replay_from_anchor` / `anchor_goal_connected` 的 BFS 只走结构边。(实现为一个 `RelationType::is_structural()` 辅助方法。)

### 无环检测(`src/graph/validation.rs`)

`ACYCLIC_RELATIONS` 从 `&[DependsOn]` 改为 `&[DependsOn, Contains]`。**`LeadsTo` 不列入**(允许环)。现有 `find_cycle_in_relation` 机制复用。

### decomposer / cascade_expand 的子任务串接

- `decomposer`(:313)、`cascade_expand`(:291)现在用 `DependsOn` 串子任务。改为:**默认 `LeadsTo`**(子任务通常是流程先后),但 prompt 告知模型可按需用 DependsOn(真正依赖)。
- decomposer 的无环检测 `find_cycle_in_relation(DependsOn)`:DependsOn 子图仍查环;LeadsTo 子图不查(允许流程回环)。

### 模型判断关系类型(prompt,`src/agent/proposer.rs` + decomposer/cascade_expand prompt + `skills/prompts/*`)

在建图相关的 prompt 里加入关系图谱指引:
- `LeadsTo`:流程/步骤的先后流向(先做 X 再做 Y);start→deliverable 主链必用此。可有环(流程回退/循环)。
- `DependsOn`:真正的依赖(组件 B 必须先存在/完成,A 才能工作)。无环。
- `Contains`:层级包含(父节点展开成子节点)。无环。
- 引导模型**先判断任务类型**(线性写作 → 纯 LeadsTo 树;系统构建 → 依赖用 DependsOn、流程用 LeadsTo),再挑边连接。

### 执行出错 → 修复 → 从 start 重走(方向校正)

复用现有 cascade/replay 机制,仅方向按新模型校正:
- 节点执行失败 → 重规划该节点
- 从 `start` 沿结构边出边重走全图,校验上游输出是否满足下游
- 上游喂的输入有问题 → 回头重规划上游 → 再从 start 重走
- 即用户最初描述的"穷举式重试直到达成目标",方向从"指向锚点"校正为"从 start 流出"

## 数据流

- 纯图模型 / agent 核心改动,不涉新增 web 端点。
- 前端关系图渲染:边方向数据变了(start→deliverable),箭头自然显示为正向流;节点 id 显示 `start`/`deliverable` 而非 A/D。`Contains` 已有钻取样式,`LeadsTo` 复用默认边样式(或给个区分色,可选)。

## 错误处理 / 边界

- 翻转方向后,P1/收敛/cascade 全部绕新方向:**改漏一处,replay/收敛会走错方向、悄悄失效**。靠回归测试兜底(见下),逐文件核对。
- 模型若给出含环的 DependsOn → 现有无环检测拦截 + 反馈重试(机制不变,只是关系集合变了)。
- 模型给出含环的 LeadsTo → 允许,replay BFS 用 visited 集合防死循环(现有 path_exists 已有 seen 集合)。

## 测试

- 单测:`start --LeadsTo--> deliverable` seed 方向正确;`anchor_goal_connected` 认 start→deliverable;`replay_from_anchor` 从 start 出发判孤儿;cascade 上游 = 入边来源。
- 单测:无环检测对 DependsOn/Contains 环报错,对 LeadsTo 环放行。
- 单测:`RelationType::is_structural()` 分类正确;`LeadsTo` 序列化/parse_wire 往返。
- 全量 `cargo test --lib` 全绿(翻转触及大量现有测试,逐一校正方向断言)。
- 端到端(pinchtab):跑一个任务,确认图是 `start → ... → deliverable` 正向流、箭头方向正确、节点名是 start/deliverable。

## 触点清单(10 文件,逐一核对方向)

`src/graph/mod.rs`(RelationType + is_structural)、`src/graph/validation.rs`(ACYCLIC_RELATIONS)、`src/agent/graph_loop.rs`(seed/path_exists/replay/anchor_goal_connected/单测)、`src/agent/cascade.rs`(dependency_predecessors_of 方向)、`src/agent/cascade_expand.rs`(子图边)、`src/agent/decomposer.rs`(子任务边 + 无环)、`src/agent/proposer.rs`(prompt + parse)、`src/web/api_runs.rs`(graph_schema required_edge_relation + prompt 文案)、`skills/prompts/*`(关系图谱指引)、`webui`(节点名/边渲染,轻量)。

## 不做(YAGNI)

- 不为 LeadsTo 设计复杂的环语义(如循环次数上限);replay 用 visited 防死循环即可。
- 不做关系类型的可视化图例大改;前端最小调整(节点名 + 箭头方向已随数据自动正确)。
- 不引入旧数据兼容层(无正式持久化数据)。
