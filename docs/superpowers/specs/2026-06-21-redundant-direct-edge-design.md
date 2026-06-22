# 冗余直连边:模型删除 + 持续监控提醒 设计

日期:2026-06-21
状态:设计已确认(逐点),待用户审阅 spec

## Context

实测报告:某 run 最终图 `11 nodes, 11 edges`,主链 `start → outline → overview → … → final-doc → deliverable`(9 个中间步骤已正确串联),**但 seed 阶段建的那条 `start→deliverable` 直连边(索引 0)仍然保留**,与主链并存。用户要求:start 是开始、deliverable 是结束,中间步骤全部在两者之间——那条绕过所有步骤的直连边应在中间步骤插入后消失,只留 `start→步骤→…→deliverable` 唯一主链。

**根因(读码坐实)**:seed 在 `graph_loop.rs:1517-1525` 强制建 `start--LeadsTo-->deliverable` 直连边;中间步骤插进来后,**全代码无任何逻辑删除它**,所以直连边长期残留。

这是「图方向/澄清」系列的收尾 bug 修复 + 一个图结构健康监控机制。

## 已确认决策(方案 A + 持续监控)

1. **模型删**(prompt 引导):filling 提示词加引导——当中间步骤插进 start 和 deliverable 之间后,用 `remove_edge_indices` 删掉原 `start→deliverable` 直连边,让主链唯一。
2. **持续监控**:建图阶段(Filling/Expanding)每步 patch 后,检测"冗余直连边"——`start→deliverable` 直连边存在 **且** start 与 deliverable 之间已有经过 ≥1 个中间节点的更长路径,则该直连边冗余。检测到 → 注入提示要求模型用 `remove_edge_indices` 删它(给出边的索引)。
3. **每轮持续提醒,不去重**:只要冗余直连边还在,**每一步都重新提醒**(不像孤儿提示那样按签名去重静默),直到模型删掉。用户明确要"持续提醒让充分运转",借此压力也能暴露模型其它问题。
4. **可扩展框架**:本期只查"冗余直连边";检测点设计成可加别的图结构形态校验(留接口,本期不实现其它)。

## 架构 / 组件落点(单文件:`src/agent/graph_loop.rs`)

### 1. 冗余直连边检测 + 每轮提醒
在 `step_graph` 的 patch 应用成功分支(现有孤儿检查块附近,Filling/Expanding 阶段),加检测:

- 找 `start`(immutable anchor)与 `deliverable`(goal,id="deliverable" 或现有 goal 识别)。
- 判断是否存在**直连边** `source==start && target==deliverable`(任意 relation,实际是 LeadsTo),记下其在 `graph.edges` 中的索引。
- 判断是否存在**绕开直连的更长路径**:从 start 出发、**不走那条直连边**,能否经 ≥1 中间节点到达 deliverable。复用 `path_exists` 的 BFS 思路,但排除直连边(或:存在另一条 start 的出边通向最终能到 deliverable 的节点)。
- 若"直连边存在 且 更长路径存在" → 直连边冗余 → 注入提示(**每轮都注入,不去重**):
  > "⚠️ 仍存在一条 start→deliverable 的直连边(索引 {i}),它绕过了所有中间步骤。主链应是 start→步骤→…→deliverable 唯一路径。请用 `propose_patch` 的 `remove_edge_indices:[{i}]` 删掉这条直连边。"

### 2. filling 提示词补充
`build_filling_hint()` 末尾加一句:"当中间步骤已串进 start→deliverable 之间,记得用 `remove_edge_indices` 删掉最初的 start→deliverable 直连边,保持主链唯一。"

### 复用
- `path_exists`(已修复,反序列化重建索引后正确)做可达性。
- `remove_edge_indices` 是 GraphPatch 已支持的字段,模型已能用。
- 检测点与现有孤儿检查同位置(patch 应用后),共用 Filling/Expanding 阶段门槛。

## 数据流
- 检测在 graph_loop 内部,提示经 `conversation.add_user` → 下一轮 Proposer 看到 → 发 remove_edge_indices 删边。
- 纯 agent 核心,无新增 web 端点。

## 错误处理 / 边界
- **任务本就无中间步骤**(start 直接到 deliverable,如极简任务):此时**没有更长路径**,直连边不算冗余 → 不提醒。这是关键边界:只有"已有中间路径 + 直连边并存"才提醒。
- 索引稳定性:`remove_edge_indices` 用的是 `graph.edges` 的当前索引;提示里给的索引必须是检测当下的真实索引(每轮重算,不缓存)。
- 模型删错/删别的边:删后下一轮重新检测,若直连边没了就不再提醒;若误删了主链边,孤儿检查会接管报相应问题。
- 持续提醒不去重可能在模型迟迟不删时刷屏:可接受(用户明确要这个强度);现有 stagnation/max_rounds 兜底防真死循环。

## 测试
- 单测:图含 `start→deliverable` 直连边 + `start→mid→deliverable` 主链 → 检测函数判定"冗余直连边存在"(返回该边索引)。
- 单测:图只有 `start→deliverable`(无中间节点)→ 不判为冗余(无更长路径)。
- 单测:删掉直连边后(只剩 start→mid→deliverable)→ 不再判冗余。
- 全量 `cargo test --lib` 全绿。
- 端到端(pinchtab):跑一个会产生多步骤的任务,填出主链后,确认 ① 出现删直连边的提示 ② 模型删掉后图里只剩 start→步骤→…→deliverable 唯一主链(无直连边)。

## 不做(YAGNI)
- 不让系统自动删直连边(方案 B)——交给模型删 + 监控提醒(用户选 A)。
- 不改 seed 不建直连边(方案 C)——会与孤儿检查/连通性兜底打架。
- 本期监控只查冗余直连边;自环、重复边等其它形态校验留接口、不实现。
