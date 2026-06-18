# 心跳自优化提示词 — Web UI 美化

对 Graph-Centric Agent 的 Web 前端 (webui/src/) 进行10轮美化优化。每轮选一个具体 UI 改进点。

---

## 设计参考 (搜索 GitHub 获取灵感)

| 项目 | 特点 |
|------|------|
| **Kanna** (github.com/jakemor/kanna) | 极简现代设计，富文本 transcript 渲染，WebSocket 实时更新 |
| **HelixUI** | 科幻 HUD 风格，沉浸式暗色主题，移动端响应式 |
| **Hermes Web UI** (7.6k stars) | 全功能仪表盘，暗/亮主题切换，SSE 流式 |
| **A2UI Vue** | 玻璃拟态 glassmorphism，Apple 级品质 |
| **Claude Code Agent UI** | 可视化关系图工作流，VueFlow 节点编辑器 |
| **OpenChamber** | 分支聊天时间线，语音模式，可定制主题 |

## 搜索外部项目

子代理有 web_search + web_fetch 工具可搜索 GitHub 查看实际代码。
用 Explore 派子代理去搜索关键词获取设计灵感和实现代码。

---

## 每轮优化方向

| 轮次 | 方向 | 示例 |
|------|------|------|
| 1 | **配色系统** | 暗色主题优化，CSS 变量体系，主题切换 |
| 2 | **排版层次** | 字体大小阶梯，消息卡片间距，行高，代码块样式 |
| 3 | **对话区域** | 消息气泡/卡片设计，用户和 AI 消息视觉区分 |
| 4 | **3D 图面板** | 节点标签清晰化，边动画，缩放控件 |
| 5 | **侧边栏** | 运行历史列表美化，状态指示器(色点) |
| 6 | **工具栏** | 图标化按钮，状态 pill，token 计数显示 |
| 7 | **设置页面** | 表单布局优化，分组卡片，保存反馈 |
| 8 | **动画过渡** | 页面切换，消息出现，loading 状态 |
| 9 | **响应式** | 移动端适配，小屏侧边栏折叠 |
| 10 | **整体 polish** | 圆角、阴影、间距一致性、hover 效果 |

### 后端 (src/web/) 配合改动

如需新的 API 端点或事件类型，同步修改后端。

---

## 子任务角色注入

- **UI 改动**: `role_prompt="你是 UI 设计专家。用 read_file 读现有组件，edit_file 替换样式和结构，write_file 创建新组件。每次改动必须产生视觉变化。优先调整 CSS 变量和组件样式，保持 Vue3 Composition API 风格。"`

- **探索参考**: `role_prompt="你是探索专家。用 web_search + web_fetch 搜索 GitHub 上的优秀 AI agent Web UI 设计，带回具体的 CSS 代码片段和设计模式。"`

---

## 每轮工作流

1. 创建 A(当前问题)和 D(优化目标)，比如 A="对话区域无层次感" D="消息卡片带角色标识和配色区分"
2. Explore 搜索外部项目获取设计灵感和代码
3. ProposePatch: 仅修改 webui/src/ 下的文件(和后端 src/web/ 如需)
4. SubAgent 执行修改(edit_file/write_file)
5. Review 通过 → 本轮完成 → 自动重启进入下一轮

---

## 约束

- 每轮只改 1-3 个文件，不引入新 npm 依赖
- 保持 Vue3 Composition API + TypeScript 风格
- 使用项目已有的 CSS 变量体系 (`var(--bg)`, `var(--accent)`, `var(--text)` 等)
- 不改 `graph/mod.rs` 和 `graph/l1.rs`(核心图结构)
- `npm run build` 必须通过
- 第 10 轮结束自动停止
