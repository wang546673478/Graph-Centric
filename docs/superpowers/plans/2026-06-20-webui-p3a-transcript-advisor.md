# WebUI P3a — 对话渲染增强 + 顾问面板 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 提升右栏对话质量——markdown/代码渲染、每条思考块独立折叠、主题感知配色、工具调用卡片化;并新增顾问问答面板,区分主力 vs 顾问消息,展示 consult_advisor 的问答。

**Architecture:** 纯前端。新增 `useMarkdown` composable(封装 `marked`)。重写 `Transcript.vue` 的渲染(markdown body、per-message thinking、主题感知、工具卡)。RunView 右栏区分 advisor 消息样式(advisor 的回答已通过 `stream:advisor` / `assistant` 进 transcript,本期用样式区分,不改数据流)。

**Tech Stack:** Vue 3 `<script setup>` + TS;新增依赖 `marked`(^18)。无测试框架(`npm run build` + 目视/pinchtab 验证)。

参考:spec `docs/superpowers/specs/2026-06-20-webui-redesign-design.md`;P2a/P2b 已完成。

读码确认的现状:
- `Transcript.vue`:thinking 用单个全局 `thinkingExpanded`(所有块一起开合);`.msg.user { background: blue }`(粗糙);thinking 块配色写死 `#fef3c7`/`#92400e`(深色下违和);`.body` 是纯文本 `<pre>`,无 markdown。
- 消息 role 已区分:`user`/`assistant`/`stream:<comp>`/`thinking:<comp>`/`thinking`/`tool_result`/`cascade`/`model`/`checkpoint`/`error`。advisor 走 `stream:advisor`→`assistant`(P2b 的 ModelWithEvents 标签)。
- webui 无 markdown 依赖;`marked` 最新 18.0.5。

## File Structure
- Modify: `webui/package.json` — 加 `marked` 依赖
- Create: `webui/src/composables/useMarkdown.ts` — 封装 marked(安全渲染 + 代码块)
- Modify: `webui/src/components/run/Transcript.vue` — markdown body + per-message thinking + 主题配色 + 工具卡 + advisor 样式
- Modify: `webui/src/styles/main.css` — 加 markdown/代码块的主题感知样式(全局,因 v-html 内容不受 scoped 约束)

---

## Task 1: 安装 marked 依赖

**Files:**
- Modify: `webui/package.json`

- [ ] **Step 1: 安装 marked**

Run:
```bash
cd /home/hhhh/Graph-Centric/webui && npm install marked@^18
```
Expected: `package.json` 的 dependencies 出现 `marked`,`package-lock.json` 更新,无报错。

- [ ] **Step 2: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功。

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/package.json webui/package-lock.json
git commit -m "build(webui): add marked for markdown rendering"
```

---

## Task 2: useMarkdown composable

**Files:**
- Create: `webui/src/composables/useMarkdown.ts`

- [ ] **Step 1: 写 useMarkdown**

创建 `webui/src/composables/useMarkdown.ts`。封装 marked,配置:GFM、换行符转 `<br>`、代码块加语言 class(供 CSS 着色)。对纯文本安全:marked 默认转义 HTML 实体,模型输出当 markdown 渲染但不执行内嵌 HTML 脚本(marked 不执行 JS,且我们不引入 raw HTML 扩展)。

```typescript
import { marked } from 'marked'

// 配置一次(模块级):GFM + 软换行转 <br>,贴合聊天气泡习惯。
marked.setOptions({
  gfm: true,
  breaks: true,
})

/**
 * 把 markdown 文本渲染为 HTML 字符串,供 v-html 使用。
 * marked 不执行脚本;输入按 markdown 解析,内嵌 raw HTML 不被特殊处理为可执行内容。
 * 对话内容来自模型,非用户可信输入也无妨——这里只做展示渲染,不注入到可执行上下文。
 */
export function renderMarkdown(text: string): string {
  if (!text) return ''
  try {
    return marked.parse(text, { async: false }) as string
  } catch {
    // 渲染失败时退回转义后的纯文本,避免破坏页面。
    const div = document.createElement('div')
    div.textContent = text
    return div.innerHTML
  }
}

export function useMarkdown() {
  return { renderMarkdown }
}
```

- [ ] **Step 2: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功,无 TS 错误。

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/composables/useMarkdown.ts
git commit -m "feat(webui): add useMarkdown composable wrapping marked"
```

---

## Task 3: markdown/代码块全局样式(主题感知)

**Files:**
- Modify: `webui/src/styles/main.css`

`v-html` 渲染出的 markdown 内容不受组件 scoped 样式约束,需在全局 `main.css` 加样式,且用 CSS 变量随主题切换。

- [ ] **Step 1: 在 main.css 末尾追加 markdown 样式**

在 `webui/src/styles/main.css` 末尾(`a:hover { ... }` 之后)追加:

```css
/* Markdown-rendered message bodies (.md-body wraps v-html output). */
.md-body { font-size: 0.85rem; line-height: 1.6; word-break: break-word; }
.md-body p { margin: 0 0 8px; }
.md-body p:last-child { margin-bottom: 0; }
.md-body h1, .md-body h2, .md-body h3 { font-size: 0.95rem; margin: 10px 0 6px; font-weight: 600; }
.md-body ul, .md-body ol { margin: 6px 0; padding-left: 20px; }
.md-body li { margin: 2px 0; }
.md-body a { color: var(--accent); }
.md-body code {
  font-family: var(--font-mono); font-size: 0.78rem;
  background: var(--bg-hover); padding: 1px 5px; border-radius: 4px;
}
.md-body pre {
  background: var(--bg-hover); border: 1px solid var(--border);
  border-radius: 6px; padding: 10px 12px; overflow-x: auto; margin: 8px 0;
}
.md-body pre code { background: none; padding: 0; font-size: 0.78rem; line-height: 1.5; }
.md-body blockquote {
  border-left: 3px solid var(--border); margin: 6px 0; padding: 2px 10px;
  color: var(--text-muted);
}
.md-body table { border-collapse: collapse; margin: 8px 0; font-size: 0.78rem; }
.md-body th, .md-body td { border: 1px solid var(--border); padding: 4px 8px; }
```

- [ ] **Step 2: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功。

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/styles/main.css
git commit -m "feat(webui): theme-aware markdown/code styles"
```

---

## Task 4: Transcript 渲染重写(markdown + per-message thinking + 主题 + advisor)

**Files:**
- Modify: `webui/src/components/run/Transcript.vue`

把整个 `Transcript.vue` 替换为下面内容。变化:
- 普通 assistant/user 消息的 body 用 markdown 渲染(`.md-body` + `v-html`)。
- thinking 块改为**每条独立折叠**(用 `Set<number>` 记录展开的索引,而非单个全局 bool)。
- 配色全部改主题感知(去掉写死的 `blue`/`#fef3c7` 等,用 CSS 变量)。
- 工具结果(`tool_result`/`model`)卡片化样式。
- advisor 消息(role 含 `advisor`,或 `stream:advisor`/`thinking:advisor`)加专属色条,区分主力。

```vue
<script setup lang="ts">
import { ref, watch, nextTick } from 'vue'
import { useI18n } from '../../composables/useI18n'
import { renderMarkdown } from '../../composables/useMarkdown'

const { t } = useI18n()
const props = defineProps<{ messages: { role: string; content: string }[]; status: string; error: string }>()
const el = ref<HTMLElement>()
// Per-message thinking expand state (by index), instead of one global toggle.
const expandedThinking = ref<Set<number>>(new Set())
function toggleThinking(i: number) {
  const s = new Set(expandedThinking.value)
  s.has(i) ? s.delete(i) : s.add(i)
  expandedThinking.value = s
}

watch(() => props.messages.length, async () => { await nextTick(); if (el.value) el.value.scrollTop = el.value.scrollHeight })

function isAdvisor(r: string): boolean {
  return r === 'advisor' || r.endsWith(':advisor')
}
function isThinking(r: string): boolean {
  return r === 'thinking' || r.startsWith('thinking:')
}
function roleClass(r: string) {
  if (isAdvisor(r)) return 'advisor'
  if (r === 'user') return 'user'
  if (r === 'assistant' || r === 'agent' || r.startsWith('stream:')) return 'assistant'
  if (r === 'tool_result') return 'tool'
  if (r === 'error') return 'error'
  if (r === 'cascade' || r === 'model' || r === 'checkpoint') return 'detail'
  return ''
}
function showRoleTag(r: string): boolean {
  return !r.startsWith('stream:') && !isThinking(r)
}
function useMd(r: string): boolean {
  // Render markdown for assistant/advisor/user prose; keep tool/detail/error monospace plain.
  return r === 'user' || r === 'assistant' || r === 'agent' || isAdvisor(r) || r.startsWith('stream:')
}
function roleLabel(r: string): string {
  if (r.startsWith('stream:')) { const c = r.slice(7); return c === 'advisor' ? '顾问' : `流式:${c}` }
  if (r.startsWith('thinking:')) { const c = r.slice(9); return `思考:${c}` }
  const zh: Record<string, string> = {
    user: '用户', assistant: '助手', agent: '代理', advisor: '顾问',
    tool_result: '工具结果', error: '错误',
    ask_user: '询问用户', block: '阻塞', explore: '探索',
    cascade: '级联回溯', model: '模型调用', checkpoint: '检查点',
    thinking: '思考',
  }
  return zh[r] || r
}
</script>

<template>
  <div class="transcript" ref="el">
    <div v-if="!messages.length" class="empty">{{ t('transcript.empty') }}</div>
    <template v-for="(m, i) in messages" :key="i">
      <!-- Thinking block: per-message collapsible -->
      <div v-if="isThinking(m.role)" class="msg thinking-msg">
        <div class="thinking-toggle" @click="toggleThinking(i)">
          {{ expandedThinking.has(i) ? '🔽' : '💭' }} {{ roleLabel(m.role) }} ({{ (m.content || '').length }} chars)
        </div>
        <pre v-if="expandedThinking.has(i)" class="thinking-body">{{ m.content }}</pre>
      </div>
      <!-- Normal message -->
      <div v-else class="msg" :class="roleClass(m.role)">
        <span v-if="showRoleTag(m.role)" class="role-tag">{{ roleLabel(m.role) }}</span>
        <div v-if="useMd(m.role)" class="md-body" v-html="renderMarkdown(m.content)"></div>
        <pre v-else class="body">{{ m.content }}</pre>
      </div>
    </template>
    <div v-if="status === 'Running' || status === 'graph'" class="thinking-indicator">💭 {{ t('transcript.thinking') }}</div>
    <div v-if="error" class="msg error"><span class="role-tag">error</span><pre class="body">{{ error }}</pre></div>
  </div>
</template>

<style scoped>
.transcript { flex: 1; overflow-y: auto; padding: 12px; }
.empty { color: var(--text-muted); font-size: 0.85rem; padding: 24px; text-align: center; }
.msg { margin: 8px 0; border-radius: 8px; padding: 8px 12px; }
.msg.user { background: var(--accent-soft); border: 1px solid var(--border); }
.msg.assistant { background: var(--bg-hover); }
.msg.advisor { background: var(--success-soft); border-left: 3px solid var(--success); }
.msg.tool { background: var(--bg-hover); border: 1px solid var(--border); }
.msg.tool .body { font-family: var(--font-mono); font-size: 0.75rem; color: var(--text-muted); }
.msg.error { border: 1px solid var(--danger); background: var(--danger-soft); }
.msg.error .body { color: var(--danger); }
.msg.detail { font-size: 0.75rem; }
.msg.detail .body { color: var(--warning); }
.role-tag { font-size: 0.65rem; text-transform: uppercase; color: var(--text-muted); margin-bottom: 4px; display: block; letter-spacing: 0.03em; }
.msg.advisor .role-tag { color: var(--success); }
.body { white-space: pre-wrap; word-break: break-word; font-size: 0.85rem; line-height: 1.6; margin: 0; font-family: var(--font); }
.thinking-indicator { padding: 8px; color: var(--text-muted); font-size: 0.8rem; font-style: italic; }
.thinking-msg { background: var(--warning-soft); border: 1px solid var(--warning); border-radius: 8px; margin: 8px 0; }
.thinking-toggle { cursor: pointer; padding: 6px 10px; font-size: 0.72rem; color: var(--warning); user-select: none; }
.thinking-toggle:hover { filter: brightness(1.1); }
.thinking-body { font-size: 0.72rem; color: var(--text-muted); white-space: pre-wrap; max-height: 300px; overflow-y: auto; padding: 4px 10px 8px; margin: 0; font-family: var(--font-mono); }
</style>
```

- [ ] **Step 2: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功,无 TS 错误。

- [ ] **Step 3: 目视/pinchtab 验证**

重建 dist 后重启 serve;打开 UI 跑一个任务,确认:
- assistant 消息里的 markdown(标题/列表/`代码`/```代码块```)正确渲染。
- 多个 thinking 块各自独立折叠(点一个不影响其它)。
- 深色/浅色切换时对话气泡、代码块、thinking 块配色都跟随,无写死浅色残留。
- 若触发 consult_advisor,顾问回答带绿色左条、标签显示"顾问"。

- [ ] **Step 4: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/components/run/Transcript.vue
git commit -m "feat(webui): markdown transcript, per-message thinking, theme-aware, advisor styling"
```

---

## Task 5: 重建 dist + 重启 + 推送

- [ ] **Step 1: 重建前端**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功。

- [ ] **Step 2: 重启 serve**

```bash
cd /home/hhhh/Graph-Centric
pid=$(pgrep -f "target/debug/serve" | head -1); [ -n "$pid" ] && kill "$pid"; sleep 1
WEB_PORT=8090 setsid ./target/debug/serve > /tmp/graph-serve.log 2>&1 < /dev/null & disown
sleep 4; curl -s -o /dev/null -w "HTTP %{http_code}\n" http://localhost:8090/
```
Expected: HTTP 200。

- [ ] **Step 3: 推送**

```bash
cd /home/hhhh/Graph-Centric
git push origin main
```

---

## 验收(P3a 整体)
- `cd webui && npm run build` 通过,无类型错误。
- 对话:assistant/user/advisor 消息走 markdown 渲染(标题/列表/代码块/行内代码),tool/detail/error 保持等宽纯文本。
- thinking 块每条独立折叠(`Set<index>`),不再全局联动。
- 深浅主题切换时,所有气泡、代码块、thinking、markdown 元素配色跟随(无写死 blue/#fef3c7)。
- 顾问消息(advisor)有专属绿色左条 + "顾问"标签,与主力消息可区分。
- 现有功能无回归(流式打字、滚动到底、错误显示)。

## 不做(留 P3b)
- 运行仪表盘(阶段指示器/token/轮次)→ P3b。
- 交互式控制(暂停/继续/中断/分支重跑按钮)→ P3b(stop/branch 端点已有,pause 需后端新增,P3b 再评估)。
- 代码块语法高亮(highlight.js)→ 暂不做;先有等宽+边框的代码块样式,语法上色按需在 P3b 评估(YAGNI)。
