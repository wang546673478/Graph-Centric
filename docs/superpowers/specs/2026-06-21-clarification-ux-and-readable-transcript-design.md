# 目标澄清交互重做 + 对话可读性 设计

日期:2026-06-21
状态:设计已确认(逐节),待用户审阅 spec

## Context

两个相关的交互缺陷,用户报告:

**A. 目标澄清交互别扭。** 当前流程:模型问 → 用户答 → 用户**点「✅ 确认开始」按钮**(发 `__CONFIRM_START__` 哨兵)→ 才建图。问题:
- 用户看到"确认开始"不知道在确认什么、开始什么。
- 点了选项卡其实已经回答了模型,再要点一个单独的"确认开始"按钮反而让人困惑——多余。
- 选项卡跨任务串台:`clarifyOptions` 是 RunView 的全局 ref,切到别的任务时上一个任务的选项还显示。
- 期望的交互(参考 superpowers 的 `AskUserQuestion`):模型抛问题 + 几个选项卡 + 永远可在输入框自己打字(等于 "Other");点选项=回答;模型问清楚了**自己**生成 deliverable 目标并开始建图(不靠用户按钮);用户看到 deliverable 后不满意还能打字纠正。

**B. 对话里冒原始 JSON。** 模型每步回复(含 `{"patch":{...},"step":"propose_patch","rationale":"..."}` 整段 JSON)经 stream_chunk 原样流式显示在对话里,不可读。后端已有"📝 {reason}"人话摘要逻辑,但原始 JSON 又被流式糊了一遍。期望:对话只显示人话摘要;原始 JSON / 模型完整输出留在调试入口(现有 Debug tab)供排查。

这是「目标澄清」(第一期已做)的交互修订,不是新功能。

## 已确认决策

### A. 澄清交互 → AskUserQuestion 式
1. 模型抛问题以**选项卡**形式呈现(ask_user 带 options),用户点卡片直接回答。
2. **删除「确认开始」按钮 + `__CONFIRM_START__` 哨兵**——点选项即回答;选项不对则输入框打字+发送(= Other)。
3. **收口信号改为"模型停止 ask_user、开始 propose_patch"**:Clarifying 阶段模型发 ask_user → 继续澄清(Paused 等答);模型发 propose_patch(建 start+deliverable)→ 自动从 Clarifying 进入 Seeding/建图。不再靠按钮或哨兵。
4. **目标可纠正**:用户看到模型建的 deliverable 节点后不满意 → 输入框打字+发送,模型据此调整(任务运行中的普通 answer 路径已支持)。
5. **修跨任务串台**:`clarifyOptions` 按 run 隔离(切换 activeRunId 时清空)。

### B. 对话只显示人话 + 调试入口
1. 对话主流每步**只显示中文人话摘要**:propose_patch → "📝 <reason>";explore → "🔍 探索:<scope/question>";ask_user → 问题 + 选项卡;ready_for_verify → "✅ 提交验证";consult_advisor → "💬 咨询顾问"。
2. **抑制原始 JSON 流式显示(方案 ii)**:Clarifying/建图阶段,模型那一步的结构化 JSON 不再通过 stream_chunk 原样进对话;只在该步解析后显示后端生成的人话摘要。thinking(reasoning_content)块保留(自然语言,可折叠,已有)。
3. **调试入口**:原始 JSON、模型完整输出、model_call 归到**已有的 Debug tab**(`DebugTimeline.vue`);主对话干净,排查时切 Debug tab 看原文。`detailMode` 开关控制详细程度(已有)。

## 架构 / 组件落点

### 后端 `src/agent/graph_loop.rs`
- **删哨兵机制**:移除 `CONFIRM_START_SENTINEL` 常量及 step_graph 入口对它的检测。
- **Clarifying 收口改信号驱动**:Clarifying 阶段——若 Proposer 返回 `ask_user` → 照常 Paused(继续澄清);若返回 `propose_patch` → 视为"模型已想清目标",`graph_phase = Seeding`(或直接让该 patch 走 seed 流程),进入建图。即:**phase 切换由 step 类型决定,不再由用户哨兵决定**。
- Clarifying 阶段的 prompt(clarifying_primed 那段)调整为:"用 ask_user 给出你对目标的理解 + 2-4 个选项(用户也可自由回答);当你确信目标清楚了,直接 propose_patch 建 start+deliverable 开始。"

### 后端 `src/web/api_runs.rs`
- **流式抑制**:在 Clarifying/建图阶段,不把模型的原始结构化输出当 stream_chunk/Transcript 推给前端(或在 transcript 事件里只推人话摘要)。保留 thinking 流。现有的 step→人话摘要逻辑(📝/🔍/✅,api_runs 内)作为对话内容来源。
- `LoopState::Paused` 的 options 已能透传(上一轮已加),前端渲染选项卡。

### 前端 `webui/src/components/run/`
- **RunView.vue**:删 `confirmStart` + Composer 的「确认开始」按钮 + `:paused` 触发的确认按钮;`clarifyOptions` 在 `activeRunId` 变化时清空(修串台);保留选项卡渲染(`.clarify-opt` 点击 = `submitTask(opt)`)+ 输入框始终可用。
- **Composer.vue**:移除 `confirmStart` emit 和「✅ 确认开始」按钮。
- **Transcript.vue**:确保只渲染人话摘要 + thinking,不渲染原始 JSON(若 stream 已在后端抑制,前端自然干净;额外加防御:content 看起来是 `{...patch...}` JSON 的消息不在主对话渲染)。
- **DebugTimeline.vue**(已有):作为原始 JSON / model_call 查看入口,无需大改;确认 model_call / 原始 step JSON 进得了 timeline。

## 数据流
- 澄清问答:`ask_user`(带 options)→ `LoopState::Paused{question,options}` → WS loop_state payload → 前端选项卡;用户点卡/打字 → `/answer` → `resume` → 下一轮。
- 收口:模型 propose_patch → graph_loop 切 Seeding → 正常建图(无哨兵、无按钮)。
- 对话内容:每步 → 后端人话摘要(Transcript)→ 前端主对话;原始 JSON → 仅 Debug tab。

## 错误处理 / 边界
- 模型在 Clarifying 一直 ask_user 不收口:沿用现有 stagnation/max_rounds 兜底。
- Heartbeat 无人值守:本就跳过 Clarifying(起始 Seeding),不受影响;无人点选项时也无影响。
- 用户切任务再切回:clarifyOptions 按 run 清空/恢复,不串台。
- 删哨兵后,旧的"用户手输 `__CONFIRM_START__`"不再有特殊含义(无正式数据依赖,安全)。

## 测试
- 单测:Clarifying 收到 ask_user → Paused(带 options);Clarifying 收到 propose_patch → 切 Seeding 建图(替代旧的哨兵测试)。
- 单测:删哨兵后,confirm 相关测试更新/移除。
- 前端构建通过;选项卡点击走 submitTask;切任务时 clarifyOptions 清空。
- 端到端(pinchtab):跑任务 → 对话出现问题+选项卡(无"确认开始"按钮)→ 点选项或打字 → 模型问清后自动建图 → 对话全程是人话摘要、无原始 JSON;切 Debug tab 能看到原始 JSON。

## 不做(YAGNI)
- 不为选项卡做复杂富交互(单选点击 + 输入框够用)。
- 不改 DebugTimeline 的现有结构(只确认原始 JSON 进得去)。
- 不引入旧哨兵的数据兼容(无正式数据)。
- B 的人话摘要复用后端已有的 step→摘要逻辑,不新设计摘要格式。
