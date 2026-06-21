# 建图阶段孤儿节点检查 + 连边引导 设计

日期:2026-06-21
状态:设计已确认(用户选方案 A),待写实现计划

## Context

实测铁证(run `ef027beb` 的 checkpoint):
```
nodes(7): ['B1','B2','B3','B4','B5','deliverable','start']
edges:    start -> deliverable : LeadsTo   ← 唯一一条边
```
模型在 Filling 阶段加了 5 个中间节点 B1–B5,但**只 add_nodes、没 add_edges**——这些节点成了孤儿,游离在图外;start 和 deliverable 之间仍是一条直接边。用户报告的"中间步骤没从 start 和 deliverable 之间长出来"正是此现象。

根因(读码确认):
1. 孤儿检测器 `replay_from_anchor()`(graph_loop.rs:2286)**只在 Task 阶段出错后调用**,建图阶段(Filling/Expanding)全程不检查孤儿。
2. `build_filling_hint()` 还在教旧的 `A → T1 → D` + `DependsOn`(第二期漏改);且 Seeding→Filling 转换提示(:1648)还写 "intermediate steps between A and D"。
3. 模型只加节点不连边,系统无任何拦截或引导。

用户已选**方案 A**:建图阶段每步查孤儿 + 提示连边 + 收口前兜底 + 修过时提示词。

## 已确认决策

- 建图阶段(Filling/Expanding)每次 propose_patch 应用后,调 `replay_from_anchor()` 检测从 start 到不了的非锚点节点。
- 有孤儿 → 注入提示,要求模型用 `LeadsTo` 边把它们接进 `start → … → deliverable` 链;提示防重复(同一批孤儿不每步刷屏)。
- 收口兜底:模型发 `ready_for_verify` 时若仍有孤儿 → 退回 Filling、要求先连边,不带孤儿进验证。
- 修过时提示词:`build_filling_hint` 与 Seeding→Filling 转换提示里的 `A/D/DependsOn/between A and D` → `start/deliverable/LeadsTo/between start and deliverable`。

## 架构 / 组件落点(单文件:`src/agent/graph_loop.rs`)

### 1. 建图阶段孤儿检查(propose_patch 应用后)
在 `step_graph` 的 `apply_patch` 成功分支(:1627 之后的 phase-transition 块内,Filling/Expanding 阶段),patch 应用 + 自动 enrich 之后,加:
- 调 `let orphans = self.replay_from_anchor();`
- 若 `!orphans.is_empty()`,且本批孤儿与上次提示的不同(防重复),注入 user 提示:列出孤儿 id,要求"用 LeadsTo 边把这些节点接进 start → … → deliverable 主链(它们现在游离、从 start 到不了)"。
- 用一个 `last_orphan_hint: Option<u64>`(孤儿 id 集合的 hash)字段防重复刷屏——孤儿集合变化时才再次提示(复用现有 `hash_string` 工具)。
- 仅在非 Seeding、图已有 ≥3 节点时检查(Seeding 只有 start/deliverable,无中间节点)。

### 2. 收口前兜底(ready_for_verify 处理)
`run_verify_and_maybe_repair`(或 ready_for_verify 的处理入口)开头,先查 `replay_from_anchor()`:
- 有孤儿 → 不进验证,`graph_phase = Filling`,注入提示"这些节点还没连进主链,先用 LeadsTo 连边再 ready_for_verify",返回 `LoopState::Running`。
- 无孤儿 → 正常进验证(现有逻辑)。

### 3. 修过时提示词
- `build_filling_hint()`:`A → T1 → D`、`DependsOn` → `start → 步骤 → deliverable`、`LeadsTo`;明确"加中间节点时必须同时用 LeadsTo 边把它接进主链"。
- Seeding→Filling 转换提示(:1648):"between A and D" → "between start and deliverable";"insert intermediate Task nodes" 保留,补"并用 LeadsTo 连边接入主链"。

## 数据流
- 纯 agent 核心改动,无新增 web 端点。
- 孤儿提示走现有 conversation.add_user → 下一轮 Proposer 看到 → 连边。
- 收口兜底复用现有 Filling 回退路径。

## 错误处理 / 边界
- `replay_from_anchor` 已防 BFS 死循环(visited 集合)。
- 孤儿提示防重复:孤儿集合 hash 未变则不重复注入(避免每步刷屏)。
- Heartbeat 模式同样适用(无人值守时孤儿提示进 conversation,模型自行连边;收口兜底同样拦截)。
- 模型反复不连边:现有 stagnation/max_rounds 兜底最终终止,不新增机制。

## 测试
- 单测:加节点不连边 → `replay_from_anchor` 返回孤儿 → step 注入连边提示(检查 conversation 含提示)。
- 单测:孤儿连边后 → `replay_from_anchor` 空 → 不再提示。
- 单测:带孤儿发 ready_for_verify → 退回 Filling(不进验证),无孤儿 → 正常进验证。
- 单测:孤儿集合不变时提示只注入一次(防重复)。
- 端到端(pinchtab):跑"写短文"任务,确认中间节点最终通过 LeadsTo 边串在 start → … → deliverable 上(图连通,非孤儿)。

## 不做(YAGNI)
- 不自动替模型连边(只检测 + 提示,连边仍由模型按语义决定哪个节点接哪个)。
- 不改 replay_from_anchor 的算法(复用现有)。
- 不为孤儿提示设计复杂升级策略(防重复 + 收口兜底足够)。
