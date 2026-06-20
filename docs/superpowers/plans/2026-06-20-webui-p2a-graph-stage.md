# WebUI P2a — 图为主舞台 + 可拖拽分隔条 + 主题感知配色 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 在 RunView 内把关系图升为主区域、对话改为可拖拽调宽的右栏;接入已实现但未使用的 2D 钻取面板并设为默认(2D/3D 可切换);两个图面板配色改为主题感知,随深浅主题切换。

**Architecture:** 纯前端。不做跨组件布局大重构(对话 hoist 到 App 右栏留待 P3),改在 RunView 内部用可拖拽分隔条实现"图为主、对话右栏可调宽"。配色统一从 CSS 变量读取,随 `useTheme` 的 `theme` ref 变化重渲染。

**Tech Stack:** Vue 3 `<script setup>` + TS;Cytoscape(2D,已在 `GraphPanel.vue`)+ Three.js(3D);无测试框架(用 `npm run build` + 目视验证)。

参考:spec `docs/superpowers/specs/2026-06-20-webui-redesign-design.md`;P1 已加 `useTheme`(导出 `theme: Ref<'light'|'dark'>`)。

发现(基于读码):RunView 当前只挂 `GraphPanel3D`,未用带钻取/面包屑的 `GraphPanel`(2D);两面板配色写死;3D 背景硬编码浅色。

---

## File Structure
- Create: `webui/src/composables/useSplitter.ts` — 通用拖拽调宽逻辑 + localStorage 持久化
- Create: `webui/src/composables/useGraphColors.ts` — 从 CSS 变量读取图配色,随主题响应
- Modify: `webui/src/components/run/RunView.vue` — 图为主区 + 可拖拽右栏 + 2D/3D 切换
- Modify: `webui/src/components/graph/GraphPanel.vue` — 配色主题感知
- Modify: `webui/src/components/graph/GraphPanel3D.vue` — 配色主题感知 + 主题切换重渲染

---

## Task 1: useSplitter composable

**Files:**
- Create: `webui/src/composables/useSplitter.ts`

- [ ] **Step 1: 写 useSplitter**

创建 `webui/src/composables/useSplitter.ts`,完整内容:

```typescript
import { ref, watch, type Ref } from 'vue'

export interface SplitterOptions {
  storageKey: string
  initial: number   // 初始像素宽
  min: number
  max: number
}

/**
 * 拖拽调宽。返回受控的 `size`(px)和一个 `startDrag(e)` 供分隔条
 * mousedown 调用。`fromRight=true` 时拖动方向反向(用于右栏:向左拖变宽)。
 */
export function useSplitter(opts: SplitterOptions, fromRight = false): {
  size: Ref<number>
  startDrag: (e: MouseEvent) => void
} {
  const saved = Number(localStorage.getItem(opts.storageKey))
  const size = ref(Number.isFinite(saved) && saved > 0 ? saved : opts.initial)
  watch(size, (v) => localStorage.setItem(opts.storageKey, String(Math.round(v))))

  function startDrag(e: MouseEvent) {
    e.preventDefault()
    const startX = e.clientX
    const startSize = size.value
    function onMove(ev: MouseEvent) {
      const dx = ev.clientX - startX
      const next = startSize + (fromRight ? -dx : dx)
      size.value = Math.max(opts.min, Math.min(opts.max, next))
    }
    function onUp() {
      window.removeEventListener('mousemove', onMove)
      window.removeEventListener('mouseup', onUp)
      document.body.style.userSelect = ''
      document.body.style.cursor = ''
    }
    document.body.style.userSelect = 'none'
    document.body.style.cursor = 'col-resize'
    window.addEventListener('mousemove', onMove)
    window.addEventListener('mouseup', onUp)
  }

  return { size, startDrag }
}
```

- [ ] **Step 2: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功,无 TS 错误。

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/composables/useSplitter.ts
git commit -m "feat(webui): add useSplitter composable for drag-to-resize panels"
```

---

## Task 2: useGraphColors composable

**Files:**
- Create: `webui/src/composables/useGraphColors.ts`

- [ ] **Step 1: 写 useGraphColors**

创建 `webui/src/composables/useGraphColors.ts`,完整内容。它从 `<html>` 计算样式读取 CSS 变量,封装图节点/边/背景用色,并暴露随 `theme` 变化重算的响应式对象:

```typescript
import { reactive, watch } from 'vue'
import { theme } from './useTheme'

export interface GraphColors {
  node: string      // 普通节点(accent)
  complex: string   // 可钻取节点环(warning)
  scope: string     // in-scope(success)
  text: string      // 标签文字
  edge: string      // 普通边
  edgeScope: string // in-scope 边
  bg: string        // 画布/3D 背景
  grid: string      // 3D 网格/边框
}

function readVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim()
  return v || fallback
}

function compute(): GraphColors {
  return {
    node: readVar('--accent', '#7c3aed'),
    complex: readVar('--warning', '#d97706'),
    scope: readVar('--success', '#059669'),
    text: readVar('--text', '#1a1a2e'),
    edge: readVar('--border', '#c4b5e0'),
    edgeScope: readVar('--success', '#059669'),
    bg: readVar('--bg', '#f5f5f0'),
    grid: readVar('--border', '#e0ddd6'),
  }
}

/**
 * 主题感知的图配色。返回一个 reactive 对象,主题切换时其字段自动更新。
 * 调用方可 `watch(() => theme.value, ...)` 触发重渲染,或直接读取最新值。
 */
export function useGraphColors(): GraphColors {
  const colors = reactive(compute())
  watch(theme, () => Object.assign(colors, compute()))
  return colors
}
```

- [ ] **Step 2: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功。

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/composables/useGraphColors.ts
git commit -m "feat(webui): add useGraphColors composable (theme-aware graph palette)"
```

---

## Task 3: 2D 面板配色主题感知

**Files:**
- Modify: `webui/src/components/graph/GraphPanel.vue`

当前 2D 面板把颜色写死为常量 `NODE_COLOR='#7c3aed'`、`COMPLEX_RING='#f59e0b'`、`SCOPE_COLOR='#059669'`,且 style 里节点文字 `'color':'#1a1a2e'`、边 `'#c4b5e0'` 等也写死。改为从 `useGraphColors()` 取色,并在主题切换时重建样式。

- [ ] **Step 1: 引入 useGraphColors 并替换写死颜色**

在 `<script setup>` 顶部 import 区(`import { ref, watch, onMounted, onUnmounted, computed } from 'vue'` 之后)加:

```typescript
import { useGraphColors } from '../../composables/useGraphColors'
import { theme } from '../../composables/useTheme'
```

删除这三行常量(原 `// ---- Cytoscape ----` 下面):
```typescript
const NODE_COLOR = '#7c3aed'
const COMPLEX_RING = '#f59e0b'
const SCOPE_COLOR = '#059669'
```
替换为:
```typescript
const C = useGraphColors()
```

- [ ] **Step 2: 用 C 重写 cytoscape style,并抽成函数**

把 `onMounted` 里 `cy = (window as any).cytoscape({ ... })` 调用中的内联 `style: [ ... ]` 数组,替换为调用一个新函数 `cyStyle()`。在 `onMounted` 之前新增该函数(用 `C` 的字段;颜色字符串改为模板):

```typescript
function cyStyle() {
  return [
    { selector: 'node', style: { 'background-color': C.node, 'label': 'data(label)', 'color': C.text, 'text-wrap': 'wrap', 'text-max-width': '100px', 'font-size': '9px', 'border-width': 1, 'border-color': C.node } },
    { selector: 'node.in-scope', style: { 'background-color': C.scope, 'border-color': C.scope, 'border-width': 3, 'text-outline-color': C.scope, 'text-outline-width': 1 } },
    { selector: 'node.complex', style: { 'border-width': 3, 'border-color': C.complex, 'border-style': 'double' } },
    { selector: 'node.selected', style: { 'border-color': C.text, 'border-width': 3, 'text-outline-color': C.text, 'text-outline-width': 1 } },
    { selector: 'edge', style: { 'width': 1.5, 'line-color': C.edge, 'target-arrow-color': C.edge, 'target-arrow-shape': 'triangle', 'curve-style': 'bezier', 'label': 'data(label)', 'font-size': '7px', 'color': C.text } },
    { selector: 'edge.in-scope', style: { 'line-color': C.edgeScope, 'target-arrow-color': C.edgeScope, 'width': 2 } },
    { selector: 'edge.Contains', style: { 'line-style': 'dashed', 'line-color': C.complex, 'target-arrow-color': C.complex } },
  ]
}
```

在 `onMounted` 里把 `style: [ ...inline... ],` 整段替换为:
```typescript
      style: cyStyle(),
```

- [ ] **Step 3: 主题切换时重建样式**

在 `onMounted(...)` 之后(`onUnmounted` 之前)加一个 watch,主题变化时把新样式套到已存在的 `cy`:

```typescript
watch(theme, () => { if (cy) { cy.style(cyStyle()); } })
```

- [ ] **Step 4: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功,无 TS 错误。

- [ ] **Step 5: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/components/graph/GraphPanel.vue
git commit -m "feat(webui): make 2D graph panel colors theme-aware"
```

---

## Task 4: 3D 面板配色主题感知

**Files:**
- Modify: `webui/src/components/graph/GraphPanel3D.vue`

当前 3D 面板把背景写死 `'#f5f5f0'`(scene.background + fog)、网格 `'#e0ddd6'/'#e8e5df'`、节点色 `'#059669'/'#f59e0b'/'#7c3aed'`、边 `'#c4b5e0'`、标签文字 `'#1a1a2e'/'#787878'`。改为从 `useGraphColors()` 取色;主题切换时更新场景背景/雾并重建图。

- [ ] **Step 1: 引入 useGraphColors + theme**

在 import 区(`import { CSS2DRenderer, CSS2DObject } ...` 之后)加:
```typescript
import { useGraphColors } from '../../composables/useGraphColors'
import { theme } from '../../composables/useTheme'
```
在 `const scopeSet = ...` 之后加:
```typescript
const C = useGraphColors()
```

- [ ] **Step 2: labelDiv 默认色改用 C.text**

把 `function labelDiv(text: string, size = '10px', color = '#1a1a2e')` 的默认参数改为读 C:
```typescript
function labelDiv(text: string, size = '10px', color = C.text): CSS2DObject {
```
并把函数体里 `div.style.textShadow = '0 0 4px #fff'` 改为:
```typescript
  div.style.textShadow = `0 0 4px ${C.bg}`
```

- [ ] **Step 3: initScene 背景/雾/网格改用 C**

在 `initScene()` 中:
- `scene.background = new THREE.Color('#f5f5f0')` → `scene.background = new THREE.Color(C.bg)`
- `scene.fog = new THREE.Fog('#f5f5f0', 15, 40)` → `scene.fog = new THREE.Fog(C.bg, 15, 40)`
- `scene.add(new THREE.GridHelper(20, 20, '#e0ddd6', '#e8e5df'))` → `scene.add(new THREE.GridHelper(20, 20, C.grid, C.grid))`

- [ ] **Step 4: 节点/边颜色改用 C**

在 `makeNodeGroup`:把
```typescript
  const c = inScope ? '#059669' : hasKids ? '#f59e0b' : '#7c3aed'
```
改为:
```typescript
  const c = inScope ? C.scope : hasKids ? C.complex : C.node
```
同函数中 torus 的 `color:'#f59e0b', emissive:'#f59e0b'` 改为 `color: C.complex, emissive: C.complex`;in-scope 光晕 `color:'#059669'` 改为 `color: C.scope`。

在 `makeEdgeGroup`:把 `const color = inScope ? '#059669' : '#c4b5e0'` 改为 `const color = inScope ? C.scope : C.edge`。

- [ ] **Step 5: 主题切换时更新背景并重建图**

在 `onMounted(...)` 之后、`onUnmounted` 之前加 watch:
```typescript
watch(theme, () => {
  if (!scene) return
  scene.background = new THREE.Color(C.bg)
  scene.fog = new THREE.Fog(C.bg, 15, 40)
  clearAll()
  updateGraph()
})
```
(`clearAll` 和 `updateGraph` 已存在;重建会用新的 C 颜色。)

- [ ] **Step 6: legend 内联浅色背景改用变量**

`<style scoped>` 里 `.legend-3d { ... background:#fff; ... color:#787878; }` 改为 `background: var(--bg-panel); color: var(--text-muted);`,`.dot.scope/.dot.node/.dot.complex` 的写死色改为 `var(--success)/var(--accent)/var(--warning)`。

- [ ] **Step 7: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功,无 TS 错误。

- [ ] **Step 8: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/components/graph/GraphPanel3D.vue
git commit -m "feat(webui): make 3D graph panel colors theme-aware"
```

---

## Task 5: RunView 图为主区 + 可拖拽右栏 + 2D/3D 切换

**Files:**
- Modify: `webui/src/components/run/RunView.vue`

目标布局:左=图(主区,占满剩余)/ 中=可拖拽分隔条 / 右=对话(可调宽,默认 380px)。图区顶部加 2D/3D 切换;默认 2D(`GraphPanel`,带钻取),可切 3D(`GraphPanel3D`)。2D 面板的 `drillDown` 是内部状态(无需父级处理);3D 面板 emit `drillDown` 事件——本期 3D 的 drill 暂不接父级(2D 才是默认钻取路径),保留其 emit 不报错即可。

- [ ] **Step 1: 引入 2D 面板、useSplitter,新增 view 切换状态**

在 `<script setup>` import 区:
- 第 7 行 `import GraphPanel3D from '../graph/GraphPanel3D.vue'` 之后加:
```typescript
import GraphPanel from '../graph/GraphPanel.vue'
import { useSplitter } from '../../composables/useSplitter'
```

在 `const tab = ref('graph')` 之后加(图视图模式 + 右栏宽度):
```typescript
const graphView = ref<'2d' | '3d'>('2d')
const { size: chatWidth, startDrag } = useSplitter(
  { storageKey: 'gc-chat-width', initial: 380, min: 280, max: 720 },
  true,
)
```

- [ ] **Step 2: 重写 template 为图主区 + 分隔条 + 对话右栏**

把整个 `<template> ... </template>` 替换为:

```html
<template>
  <div class="run-view">
    <!-- 图主舞台 -->
    <div class="graph-stage">
      <div class="stage-tabs">
        <div class="view-toggle">
          <button :class="{ active: graphView === '2d' }" @click="graphView = '2d'">2D</button>
          <button :class="{ active: graphView === '3d' }" @click="graphView = '3d'">3D</button>
        </div>
        <button :class="{ active: tab === 'graph' }" @click="tab = 'graph'">{{ t('graph.tab') }}</button>
        <button :class="{ active: tab === 'debug' }" @click="tab = 'debug'">Debug</button>
      </div>
      <template v-if="tab === 'graph'">
        <GraphPanel v-if="graphView === '2d'" :key="(activeRunId || 'empty') + '-2d'"
          :nodes="nodes" :edges="edges" :scopeNodeIds="scopeNodeIds" />
        <GraphPanel3D v-else :key="(activeRunId || 'empty') + '-3d'"
          :nodes="nodes" :edges="edges" :scopeNodeIds="scopeNodeIds" />
      </template>
      <DebugTimeline v-else-if="tab === 'debug'" />
    </div>

    <!-- 可拖拽分隔条 -->
    <div class="splitter" @mousedown="startDrag"></div>

    <!-- 对话右栏(可调宽) -->
    <div class="chat-panel" :style="{ width: chatWidth + 'px' }">
      <Transcript :messages="transcript" :status="status" :error="errorMsg" />
      <div class="toolbar">
        <button v-if="status === 'Running'" class="danger" @click="stopRun">{{ t('run.stop') }}</button>
        <span class="run-label" v-if="activeRunId">{{ activeRunId.slice(0,8) }}… · {{ status }}</span>
      </div>
      <Composer :disabled="sending" @send="submitTask" />
    </div>
  </div>
</template>
```

- [ ] **Step 3: 替换 style**

把整个 `<style scoped> ... </style>` 替换为:

```html
<style scoped>
.run-view { display: flex; flex: 1; min-height: 0; }
.graph-stage { flex: 1; min-width: 0; display: flex; flex-direction: column; background: var(--bg); }
.stage-tabs { display: flex; align-items: center; gap: 4px; border-bottom: 1px solid var(--border); background: var(--bg-panel); padding: 0 6px; }
.stage-tabs > button { padding: 8px 10px; background: none; color: var(--text-muted); border-radius: 0; font-size: 0.8rem; }
.stage-tabs > button.active { color: var(--accent); border-bottom: 2px solid var(--accent); font-weight: 500; }
.view-toggle { display: flex; gap: 2px; margin-right: auto; padding: 4px 0; }
.view-toggle button { padding: 2px 10px; font-size: 0.7rem; border: 1px solid var(--border); background: var(--bg); color: var(--text-muted); border-radius: 4px; }
.view-toggle button.active { background: var(--accent); color: #fff; border-color: var(--accent); }
.splitter { width: 5px; flex-shrink: 0; cursor: col-resize; background: var(--border); transition: background 0.1s; }
.splitter:hover { background: var(--accent); }
.chat-panel { flex-shrink: 0; display: flex; flex-direction: column; min-width: 0; border-left: 1px solid var(--border); background: var(--bg-panel); }
.toolbar { display: flex; align-items: center; gap: 8px; padding: 4px 12px; border-top: 1px solid var(--border); background: var(--bg); }
.toolbar button { font-size: 0.75rem; padding: 4px 10px; }
.run-label { font-size: 0.7rem; color: var(--text-muted); font-family: var(--font-mono); }
</style>
```

- [ ] **Step 4: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功,无 TS 错误。

- [ ] **Step 5: 目视验证**

确保前端 dev 在跑(`cd webui && BACKEND_PORT=8090 npm run dev -- --host 0.0.0.0`),打开 http://192.168.31.228:5173:
- 进入运行页:图占据左侧主区,对话在右侧。
- 拖动中间分隔条:对话栏实时变宽/变窄,图区相应缩放;刷新后宽度保持。
- 2D/3D 切换按钮可切换面板;默认 2D,2D 下可点可钻取节点(橙色环)进入子图、面包屑跳回。
- 切深浅主题:两种图面板的节点/边/背景颜色随之变化。

- [ ] **Step 6: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/components/run/RunView.vue
git commit -m "feat(webui): graph-centered stage with draggable chat rail and 2D/3D toggle"
```

---

## Task 6: 推送 P2a

- [ ] **Step 1: 推送**

```bash
cd /home/hhhh/Graph-Centric
git push origin main
```

---

## 验收(P2a 整体)
- `cd webui && npm run build` 通过,无类型错误。
- 运行页:关系图为左侧主区,对话为右侧可拖拽调宽栏(宽度记住),拖分隔条实时调整。
- 2D/3D 切换;默认 2D,钻取/面包屑/详情卡可用。
- 深浅主题切换时,2D 与 3D 面板的配色(节点/边/背景/网格/标签)均随之更新。
- 现有功能无回归:发任务、停止、流式对话、Debug tab 仍可用。

## 不做(留 P2b/P3)
- 后端 `GraphPatch` 增量事件、实时构建动画(节点随 agent 浮现/活动高亮/失败闪红)→ P2b。
- 把对话 hoist 到 App 级右栏的跨组件重构 → P3(本期在 RunView 内实现,效果等价)。
- 3D 面板的 drill-down 接父级 → 暂不做(2D 是默认钻取路径)。
