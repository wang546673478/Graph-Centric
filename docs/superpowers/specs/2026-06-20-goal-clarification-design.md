# 目标澄清阶段(Clarifying Phase)设计

日期:2026-06-20
状态:设计已确认,待写实现计划

## Context

当前 agent 任务进来后**直接建图**(Seeding 阶段 seed A→D),从不和用户确认"目标到底是什么"。用户的核心诉求:每个任务开始都应像和 Claude Code 讨论一样——模型先抛出自己的疑问、给几个选项 + "其他"让用户补充,来回几轮**确认目标**,然后才建图。

相关背景:
- `ask_user` 几乎不被使用、`paused` 看似用不上——根因是 intake 闸门被软化为只记日志([[feedback_intake_gate_softening]]),Proposer 极少发 AskUser。但底层 `Paused → POST /answer → gl.resume()` 管线是**通的**(已读码验证:api_runs.rs:809 + post_answer:216)。
- 这是「图方向翻转(LeadsTo)」改造的**第一期**(按风险递进拆开,详见 [[graph-direction-and-goal-clarification]])。图方向翻转是高风险的核心不变量改动,单独第二期 spec,本期不含。

## 已确认的设计决策

1. **触发**:每个任务开始都先澄清目标(Heartbeat 无人值守模式除外,见下)。
2. **收场**:用户**点「✅ 确认开始」**才退出澄清、进入建图(用户掌控收场时机,非模型自判断)。
3. **澄清形式**:选项与文本混合——模型能给选项时输出「我理解的目标 + 选项 + 其他」,不适合时出纯文本问题。前端两种都支持。
4. **呈现位置**:复用右侧对话栏(Transcript),问题 + 选项按钮内联在对话流里。
5. **执行中不打扰用户**:澄清阶段**之后**,Proposer 遇到疑问用 `consult_advisor`(顾问)或自决,不再向用户 AskUser。

## 架构

### 新增 FSM 前置阶段 `GraphPhase::Clarifying`

`GraphPhase` 枚举加 `Clarifying`,排在 `Seeding` 之前。loop 起始阶段从 `Seeding` 改为 `Clarifying`(Heartbeat 模式除外)。

- **Clarifying 阶段行为**:Proposer 被约束为只发 `AskUser`(澄清目标),不建图。每轮:模型输出目标理解 + 选项/问题 → `LoopState::Paused` → 用户答 → `gl.resume(answer)` → 下一轮。
- **退出**:用户答案等于退出哨兵 `__CONFIRM_START__`(前端「✅ 确认开始」按钮发送)→ graph_loop 检测到 → 把累积的目标语境留在 conversation → 切到 `Seeding` → 正常建 A→D。
- **Heartbeat 模式**:无人值守,跳过 Clarifying(直接起始于 Seeding),保持现有自动优化循环不被阻塞。

## 组件落点

- **`src/agent/graph_loop.rs`**
  - `GraphPhase` 加 `Clarifying` 变体
  - 起始阶段:非 heartbeat → `Clarifying`;heartbeat → `Seeding`(沿用现状)
  - `step_graph` 加 Clarifying 分支:期望 AskUser;收到 AskUser → Paused;收到答案若为 `__CONFIRM_START__` → 转 Seeding,否则继续澄清
  - 退出哨兵常量 `CONFIRM_START_SENTINEL = "__CONFIRM_START__"`
- **`src/agent/proposer.rs`**
  - Clarifying 阶段的 system prompt 块:指示"先澄清目标,给选项+其他或纯文本问题,绝不 propose_patch";复用现有 `ProposerStep::AskUser`(已带 `options`)
  - 执行阶段(Seeding 之后)prompt:遇疑问用 consult_advisor/自决,不 AskUser
- **`webui/src/components/run/Transcript.vue`**:AskUser options 已能渲染按钮;确保澄清问答正常显示(role=ask_user 已有样式)
- **`webui/src/components/run/Composer.vue`** 或 Transcript:在澄清进行时显示固定的「✅ 确认开始」按钮,点击经 `/answer` 发送 `__CONFIRM_START__`;options 选项点击也走 `/answer`

## 数据流

- 澄清问答全走现有 WebSocket:`Paused` 状态 + `transcript`(role=ask_user)+ `POST /api/runs/:id/answer`。**零新增后端端点**。
- 「确认开始」是一次特殊的 `/answer`(body=`__CONFIRM_START__`);其余答案是普通文本/选项。
- 确认后,澄清对话已在 `conversation` 里,Seeding 的 Proposer 据此 seed A→D。

## 错误处理 / 边界

- 用户长时间不确认:沿用现有 stagnation/stuck/max_rounds 兜底,不新增机制。
- Heartbeat:跳过 Clarifying(起始 Seeding),自动化不受影响。
- 已有 run 恢复 / 分支重跑:从 checkpoint 恢复时若已过 Clarifying,直接进后续阶段(checkpoint 记录的 phase 决定)。
- 哨兵冲突:`__CONFIRM_START__` 作为普通用户输入的概率极低;前端只在「确认开始」按钮发它,手输同样字符串等效于确认(可接受)。

## 测试

- 单测:Clarifying 收到 AskUser → Paused;收到 `__CONFIRM_START__` → 转 Seeding 并建图;收到普通答案 → 继续 Clarifying。
- 单测:Heartbeat 模式起始阶段为 Seeding(跳过 Clarifying)。
- 端到端(pinchtab):新建任务 → 对话栏出现目标澄清问题+选项 → 答几轮 → 点「确认开始」→ 图开始构建。

## 不做(YAGNI / 留后续)

- 图方向翻转为 LeadsTo:高风险核心改动,**第二期单独 spec**(本期不动方向)。
- 澄清的独立面板/弹层:复用对话栏即可,不新建组件。
- 执行中向用户提问的复杂策略:本期只约束"不 AskUser、用 consult_advisor/自决",不做更细的升级路径。
