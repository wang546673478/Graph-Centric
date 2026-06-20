# WebUI P1 — 双主题 + 三栏布局地基 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 webui 加上深浅双主题(默认深色、可一键切换、记住偏好)和三栏布局骨架(左导航 / 中央关系图舞台占位 / 右对话栏),现有所有功能保持可用。

**Architecture:** 纯前端改动。主题靠 `<html data-theme>` + CSS 变量切换,偏好存 localStorage,由新 composable `useTheme` 管理。布局在 `App.vue` 重构为三栏 flex,折叠状态存 localStorage。中央舞台本期仅放占位 + 现有 router-view,真正的关系图主舞台在 P2 做。

**Tech Stack:** Vue 3 `<script setup>` + TypeScript,Vite,CSS 变量。无测试框架(用 `npm run build` + 目视验证)。

参考 spec:`docs/superpowers/specs/2026-06-20-webui-redesign-design.md`

---

## File Structure

- Create: `webui/src/composables/useTheme.ts` — 主题状态 + localStorage 持久化 + 应用到 `<html>`
- Modify: `webui/src/styles/main.css` — 现有变量保留为浅色根,新增 `[data-theme="dark"]` 深色变量集
- Modify: `webui/src/main.ts` — 启动时初始化主题(在 mount 前应用,避免闪白)
- Modify: `webui/src/components/shared/TopBar.vue` — 加主题切换按钮
- Modify: `webui/src/App.vue` — 三栏布局骨架 + 折叠状态
- Modify: `webui/src/locales/en.ts` / `webui/src/locales/zh-CN.ts` — 加主题/折叠相关文案(若现有文案结构需要)

---

## Task 1: 深色主题 CSS 变量集

**Files:**
- Modify: `webui/src/styles/main.css:1-22`(`:root` 变量块)

- [ ] **Step 1: 把现有 `:root` 变量改为浅色显式声明,并新增深色变量集**

在 `main.css` 顶部,把现有 `:root { ... }`(第 1–22 行)替换为下面内容。浅色变量值保持与现状一致(只是语义上成为"light"默认),新增 `[data-theme="dark"]`:

```css
:root, [data-theme="light"] {
  --bg: #f5f5f0;
  --bg-panel: #ffffff;
  --bg-hover: #f0ede8;
  --border: #e0ddd6;
  --text: #1a1a2e;
  --text-muted: #787878;
  --accent: #7c3aed;
  --accent-hover: #6d28d9;
  --accent-soft: #f5f3ff;
  --danger: #dc2626;
  --danger-soft: #fef2f2;
  --success: #059669;
  --success-soft: #ecfdf5;
  --warning: #d97706;
  --warning-soft: #fffbeb;
  --shadow: 0 1px 3px rgba(0,0,0,0.06), 0 1px 2px rgba(0,0,0,0.04);
  --shadow-md: 0 4px 6px rgba(0,0,0,0.05), 0 2px 4px rgba(0,0,0,0.04);
  --radius: 8px;
  --font: 'Inter', -apple-system, 'Segoe UI', system-ui, sans-serif;
  --font-mono: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
}

[data-theme="dark"] {
  --bg: #0f1117;
  --bg-panel: #151823;
  --bg-hover: #1c2030;
  --border: #232838;
  --text: #e5e7eb;
  --text-muted: #8b8fa3;
  --accent: #a78bfa;
  --accent-hover: #b9a3fc;
  --accent-soft: #1e1b3a;
  --danger: #f87171;
  --danger-soft: #2a1517;
  --success: #34d399;
  --success-soft: #0f2922;
  --warning: #fbbf24;
  --warning-soft: #2a2310;
  --shadow: 0 1px 3px rgba(0,0,0,0.4), 0 1px 2px rgba(0,0,0,0.3);
  --shadow-md: 0 4px 12px rgba(0,0,0,0.5), 0 2px 4px rgba(0,0,0,0.3);
  --radius: 8px;
  --font: 'Inter', -apple-system, 'Segoe UI', system-ui, sans-serif;
  --font-mono: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
}

html { color-scheme: light dark; }
```

保留文件其余部分(`* { box-sizing }`、`body`、滚动条、`.status-pill`、`button`、`input`、`a`)不变——它们已经全部用变量,自动跟随主题。

- [ ] **Step 2: 构建验证**

Run: `cd webui && npm run build`
Expected: 构建成功,无 CSS 报错。

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/styles/main.css
git commit -m "feat(webui): add dark theme CSS variable set"
```

---

## Task 2: useTheme composable

**Files:**
- Create: `webui/src/composables/useTheme.ts`

- [ ] **Step 1: 写 useTheme composable**

创建 `webui/src/composables/useTheme.ts`,完整内容:

```typescript
import { ref, watch } from 'vue'

export type Theme = 'light' | 'dark'

const STORAGE_KEY = 'gc-theme'

function initialTheme(): Theme {
  const saved = localStorage.getItem(STORAGE_KEY)
  if (saved === 'light' || saved === 'dark') return saved
  return 'dark' // 默认深色
}

// 模块级单例,全应用共享同一份主题状态。
export const theme = ref<Theme>(initialTheme())

/** 把当前主题写到 <html data-theme>。在 mount 前调用一次,避免首帧闪白。 */
export function applyTheme(t: Theme = theme.value) {
  document.documentElement.setAttribute('data-theme', t)
}

// 主题变化时同步 DOM + localStorage。
watch(theme, (t) => {
  applyTheme(t)
  localStorage.setItem(STORAGE_KEY, t)
})

export function useTheme() {
  function toggleTheme() {
    theme.value = theme.value === 'dark' ? 'light' : 'dark'
  }
  return { theme, toggleTheme }
}
```

- [ ] **Step 2: 在 main.ts 启动时应用主题**

修改 `webui/src/main.ts`:在 import 区加入 `applyTheme`,并在 `createApp(...).mount(...)` 之前调用一次。

把第 1–10 行的 import 区末尾(`import './styles/main.css'` 之后)加一行:

```typescript
import { applyTheme } from './composables/useTheme'
import './styles/main.css'
```

把最后一行 `createApp(App).use(router).mount('#app')` 替换为:

```typescript
applyTheme() // 在挂载前应用主题,避免首帧闪白
createApp(App).use(router).mount('#app')
```

- [ ] **Step 3: 构建验证**

Run: `cd webui && npm run build`
Expected: 构建成功,无 TS 类型错误。

- [ ] **Step 4: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/composables/useTheme.ts webui/src/main.ts
git commit -m "feat(webui): add useTheme composable with localStorage persistence"
```

---

## Task 3: TopBar 主题切换按钮

**Files:**
- Modify: `webui/src/components/shared/TopBar.vue`

- [ ] **Step 1: 在 TopBar 加主题切换按钮**

修改 `webui/src/components/shared/TopBar.vue`。

在 `<script setup>` 区,把 import `useI18n` 那行下面加 `useTheme` 导入,并在 setup 里解构。具体:第 4 行 `import { useI18n, locale } ...` 之后加:

```typescript
import { useTheme } from '../../composables/useTheme'
```

在 `const router = useRouter()`(第 9 行)之后加:

```typescript
const { theme, toggleTheme } = useTheme()
```

在 `<template>` 的 `.right` div 内,`<DetailModeToggle />`(第 34 行)之前插入主题切换按钮:

```html
      <button class="theme-btn" @click="toggleTheme"
        :title="theme === 'dark' ? '切换到浅色' : '切换到深色'">
        {{ theme === 'dark' ? '☀️' : '🌙' }}
      </button>
```

在 `<style scoped>` 末尾(第 59 行 `.lang-btn:hover` 之后)加按钮样式:

```css
.theme-btn {
  background: var(--bg-hover); border: 1px solid var(--border);
  padding: 2px 8px; border-radius: 4px; font-size: 0.85rem; cursor: pointer;
  line-height: 1;
}
.theme-btn:hover { border-color: var(--accent); }
```

- [ ] **Step 2: 构建验证**

Run: `cd webui && npm run build`
Expected: 构建成功。

- [ ] **Step 3: 目视验证(开发服务器)**

Run: `cd webui && BACKEND_PORT=8090 npm run dev -- --host 0.0.0.0`(若已在跑则跳过)
打开 http://192.168.31.228:5173,确认:默认深色;点 ☀️/🌙 切换深浅;刷新页面后主题保持不变(localStorage 生效)。

- [ ] **Step 4: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/components/shared/TopBar.vue
git commit -m "feat(webui): add theme toggle button to top bar"
```

---

## Task 4: 三栏布局骨架

**Files:**
- Modify: `webui/src/App.vue`

- [ ] **Step 1: 重构 App.vue 为三栏布局 + 折叠状态**

把 `webui/src/App.vue` 整个文件替换为下面内容。三栏 = 左 Sidebar(可折叠)/ 中央主区(放现有 router-view,P2 改为关系图舞台)/ 右栏占位(本期为空壳,P3 填对话面板)。折叠状态存 localStorage。

```vue
<script setup lang="ts">
import { onMounted, provide, ref, watch } from 'vue'
import Sidebar from './components/layout/Sidebar.vue'
import TopBar from './components/shared/TopBar.vue'
import { activeRunId, detailMode, runs, loadRuns } from './composables/useRunSocket'

provide('activeRunId', activeRunId)
provide('detailMode', detailMode)
provide('runs', runs)

const SIDEBAR_KEY = 'gc-sidebar-collapsed'
const RIGHT_KEY = 'gc-right-collapsed'
const sidebarCollapsed = ref(localStorage.getItem(SIDEBAR_KEY) === '1')
const rightCollapsed = ref(localStorage.getItem(RIGHT_KEY) === '1')
watch(sidebarCollapsed, (v) => localStorage.setItem(SIDEBAR_KEY, v ? '1' : '0'))
watch(rightCollapsed, (v) => localStorage.setItem(RIGHT_KEY, v ? '1' : '0'))

onMounted(() => { loadRuns() })
</script>

<template>
  <div class="app-shell">
    <aside class="col-left" :class="{ collapsed: sidebarCollapsed }">
      <Sidebar />
    </aside>
    <button class="rail-toggle left" @click="sidebarCollapsed = !sidebarCollapsed"
      :title="sidebarCollapsed ? '展开导航' : '收起导航'">
      {{ sidebarCollapsed ? '›' : '‹' }}
    </button>

    <div class="col-center">
      <TopBar />
      <main class="main-content"><router-view /></main>
    </div>

    <button class="rail-toggle right" @click="rightCollapsed = !rightCollapsed"
      :title="rightCollapsed ? '展开侧栏' : '收起侧栏'">
      {{ rightCollapsed ? '‹' : '›' }}
    </button>
    <aside class="col-right" :class="{ collapsed: rightCollapsed }">
      <div class="right-placeholder">
        <p>对话 / 顾问面板</p>
        <span>P3 在此填充</span>
      </div>
    </aside>
  </div>
</template>

<style scoped>
.app-shell { display: flex; height: 100vh; overflow: hidden; background: var(--bg); }
.col-left {
  width: 220px; flex-shrink: 0; overflow: hidden;
  transition: width 0.18s ease;
}
.col-left.collapsed { width: 0; }
.col-center { flex: 1; display: flex; flex-direction: column; min-width: 0; }
.main-content { flex: 1; overflow-y: auto; display: flex; flex-direction: column; }
.col-right {
  width: 340px; flex-shrink: 0; overflow: hidden;
  background: var(--bg-panel); border-left: 1px solid var(--border);
  transition: width 0.18s ease;
}
.col-right.collapsed { width: 0; border-left: none; }
.right-placeholder {
  padding: 20px 16px; color: var(--text-muted); font-size: 0.8rem;
  display: flex; flex-direction: column; gap: 4px;
}
.right-placeholder span { font-size: 0.7rem; opacity: 0.6; }
.rail-toggle {
  width: 14px; flex-shrink: 0; background: var(--bg-panel);
  border: none; border-right: 1px solid var(--border);
  color: var(--text-muted); cursor: pointer; font-size: 0.7rem; padding: 0;
}
.rail-toggle.right { border-right: none; border-left: 1px solid var(--border); }
.rail-toggle:hover { color: var(--accent); background: var(--bg-hover); }
</style>
```

- [ ] **Step 2: 构建验证**

Run: `cd webui && npm run build`
Expected: 构建成功,无 TS 错误。

- [ ] **Step 3: 目视验证**

打开 http://192.168.31.228:5173,确认:
- 三栏可见(左导航 / 中间内容 / 右侧占位栏)。
- 点左右两条窄边的箭头按钮可折叠/展开对应栏,有过渡动画。
- 刷新后折叠状态保持。
- 现有所有页面(运行/用量/技能/设置)仍可通过顶栏导航正常打开。

- [ ] **Step 4: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/App.vue
git commit -m "feat(webui): three-column layout shell with collapsible side rails"
```

---

## Task 5: 推送 P1 到 GitHub

- [ ] **Step 1: 推送**

```bash
cd /home/hhhh/Graph-Centric
git push origin main
```

Expected: 推送成功,P1 全部提交上 GitHub。

---

## 验收(P1 整体)

- `cd webui && npm run build` 通过,无类型/CSS 错误。
- 默认深色主题;☀️/🌙 一键切换深浅;刷新记住偏好。
- 三栏布局:左导航、中央内容、右占位栏;左右栏可折叠并记住状态。
- 现有所有 View(运行、历史、用量、技能、文件、设置)经顶栏导航仍正常打开,无功能回归。
- i18n、StatusPill、DetailModeToggle 等现有顶栏元素保持可用。
