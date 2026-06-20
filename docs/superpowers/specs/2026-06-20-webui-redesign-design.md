# Graph-Centric WebUI 改造设计

日期:2026-06-20
状态:设计已确认,待写实现计划

## Context

现有 webui(Vue 3 + Vite)功能完整但视觉朴素:扁平米白主题、基础侧边栏、组件简单。已有结构:运行视图、2D/3D 关系图面板、对话记录、设置、历史、技能、文件、用量、检查点树、中英双语(`useI18n`)、WebSocket 实时流(`useRunSocket`)。

用户要求:**视觉重做 + 加新功能**,参考 openclaw 等开源 AI agent webui,重点覆盖对话体验、关系图面板、运行可观测、整体外观四个方面。

核心约束:关系图是项目招牌,体现"每个任务都是关系图上的操作"这一核心思想——**分形/无限层**(复杂节点展开成子图,可无限深,最终目标始终是第一层)。

## 已确认的设计决策

1. **主题**:深浅双主题,顶栏一键切换,默认深色("深蓝灰底 + 紫色高亮",突出关系图发光)。偏好存 `localStorage`。
2. **布局**:关系图升为中央主舞台。三栏 = 左导航栏(可折叠)+ 中央关系图 + 右对话/日志栏(可调宽、可折叠)。
3. **关系图**:实时构建动画(节点随 agent 每步浮现、活动节点发光脉冲、失败重规划闪红替换、按 kind/状态着色、点节点弹 L1 详情卡)。
4. **多层导航**:钻取式 + 面包屑。点展开节点 → 放大进子图,面包屑把"第一层目标"钉在最左,随时跳回任意层。
5. **新功能**:对话渲染增强、运行仪表盘、顾问问答面板、交互式控制——全部要。

## 架构与组件

### 主题系统
- 扩展 `src/styles/main.css`:现有 CSS 变量保留为浅色,新增 `[data-theme="dark"]` 深色变量集。
- 新增 `src/composables/useTheme.ts`:读写 `localStorage`、切换 `<html data-theme>`、默认深色。
- 顶栏 `TopBar.vue` 加主题切换按钮。

### 布局骨架(`App.vue` 重构)
- 三栏 flex 布局:`Sidebar`(左,可折叠)| `GraphStage`(中,主舞台)| `RightPanel`(右,可折叠/调宽)。
- 折叠状态存 `localStorage`。
- 移动端响应式:窄屏时右栏变抽屉、导航栏变汉堡菜单(二期可细化)。

### 关系图主舞台(新 `src/components/graph/GraphStage.vue`)
- **基座**:默认 2D 力导向(d3-force,动画可控、性能稳),保留 3D 作可切换"炫酷视图"。复用现有 `GraphPanel.vue` / `GraphPanel3D.vue` 逻辑,重构为受 `GraphStage` 调度。
- **实时构建动画**:订阅 `useRunSocket` 的图 patch 事件,增量更新而非整图重渲染。新节点淡入+缩放弹出,边描边动画,活动节点紫色光晕呼吸,失败节点闪红→淡出→新节点浮现。
- **着色**:anchor 紫、task 蓝、完成绿、失败红、其它灰。
- **多层钻取**:新 `src/composables/useGraphDrill.ts` 维护当前层栈 + 面包屑。点已展开节点(`node.expanded`)平滑放大进子图;面包屑组件 `GraphBreadcrumb.vue` 显示"第一层目标 / … / 当前层",点任意段跳回。
- **节点详情卡** `NodeDetailCard.vue`:点节点弹出 L1(职责/实现/约束),复用现有 L1 数据。

### 右侧栏(新 `src/components/panel/RightPanel.vue`,tab 切换)
- **对话流 tab**(增强现有 `Transcript.vue`):
  - markdown 渲染(`marked`)+ 代码高亮(`highlight.js`)。
  - 思考块(reasoning_content)可折叠。
  - 工具调用卡片化:工具名 + 参数 + 结果摘要。
  - 流式打字效果保留。
- **顾问 tab**(新 `AdvisorPanel.vue`):区分主力/顾问消息(不同色条),展示"主力问 → 顾问答",订阅 WebSocket `advisor` 标签事件(对应已实现的 `consult_advisor`)。

### 运行仪表盘(新 `src/components/run/RunDashboard.vue`)
- 细条置于右栏顶部或顶栏下方。
- 阶段指示器:Graph→Task→Review→Done,高亮当前阶段。
- 轮次计数、token 成本累计、运行时长。数据来自现有 `Status` 事件。

### 交互式控制(`TopBar.vue` + 复用 `CheckpointTree.vue`)
- 运行控制按钮:暂停 / 继续 / 中断(调用现有 `/api/runs/*` 端点;若端点缺失则后端补)。
- 从检查点分支重跑:复用现有 `CheckpointTree`。
- 手动编辑图节点 → 三期(本设计不含)。

## 数据流
- 所有实时数据继续走 `useRunSocket` 的 WebSocket(`StreamChunk`/`Status`/图 patch/`advisor` 事件)。
- 主题、折叠状态、面包屑层栈为前端本地状态,不涉后端。
- 图 patch 事件需后端确认会推送增量(若现在只推全量,后端加增量事件 → 在实现计划中核实)。

## 分期(便于分阶段验收)

- **P1 · 地基**:双主题系统 + 三栏布局骨架 + 顶栏主题切换。现有功能全部保留可用。
- **P2 · 关系图主舞台**:GraphStage + 实时构建动画 + 着色 + 多层钻取/面包屑 + 节点详情卡。
- **P3 · 新功能**:对话渲染增强 + 顾问面板 + 运行仪表盘 + 交互式控制。

## 验证
1. 每期 `cd webui && npm run build` 通过,无类型错误。
2. P1:深浅主题切换正常、刷新记住偏好、三栏折叠正常、现有所有 View 仍可用。
3. P2:启动一个 run,观察节点随 agent 实时浮现、活动节点高亮、失败闪红;点展开节点进子图、面包屑跳回;2D/3D 切换。
4. P3:对话 markdown/代码/思考块/工具卡渲染正确;`consult_advisor` 触发时顾问 tab 显示问答;仪表盘阶段/token/轮次实时更新;暂停/继续/中断/分支重跑可用。
5. 端到端:`npm run dev` + 后端 8090,真实跑一个任务走查 P1–P3 全链路。

## 不做(YAGNI)
- 手动拖拽编辑图节点(留三期)。
- 移动端深度适配(P1 仅做基础响应式)。
- 与现有 `GraphPanel.vue` 无关的重构。
