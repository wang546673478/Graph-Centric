# WebUI P3b — 运行仪表盘 + 交互式控制 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 在运行视图加一个轻量仪表盘(阶段指示器 Graph→Task→Review→Done + token 成本 + 轮次 + 时长),并补齐交互式控制(停止已有;新增「从最新检查点分支重跑」)。

**Architecture:** 纯前端。新增 `RunDashboard.vue`(从已有的 store + WS 事件读阶段/token/轮次/时长)。RunView 维护 `round`(来自 checkpoint 事件)并挂载仪表盘 + 控制按钮。分支重跑调用已有的 `POST /runs/:id/branch` 端点。**不新增后端**(暂停运行中 run 无端点,按 spec 推迟;stop + branch 已覆盖主要控制)。

**Tech Stack:** Vue 3 `<script setup>` + TS。无测试框架(`npm run build` + pinchtab 验证)。

参考:spec `docs/superpowers/specs/2026-06-20-webui-redesign-design.md`;P3a 已完成。

读码确认的现状:
- `Status` 事件:`phase`/`message`/`tokens_used`(**无 round**)。`checkpoint` 事件:`index`/`round`/`node_count`/`edge_count`(前端 WSEvent 已含这些字段)。轮次从 checkpoint 取。
- RunView 已有 `stopRun()`(DELETE /runs/:id),store 有 `status`/`tokensUsed`,run 列表项有 `duration_sec`。
- `POST /runs/:id/branch`,body `{from_checkpoint: number}`,返回 `{id: <new>}`(或含 id 的 JSON)。
- `CheckpointTree.vue` 是 2 行空壳——本期不依赖它,分支重跑直接用最新 checkpoint index。
- store 当前不存 round 和 checkpoint 数;需在 RunView 的 WS handler 里补存。

## File Structure
- Modify: `webui/src/composables/useRunSocket.ts` — store 增加 `round` 和 `lastCheckpoint` 字段
- Create: `webui/src/components/run/RunDashboard.vue` — 阶段指示器 + token + 轮次 + 时长
- Modify: `webui/src/components/run/RunView.vue` — WS handler 记录 round/checkpoint;挂载仪表盘;加「分支重跑」按钮

---

## Task 1: store 增加 round / lastCheckpoint 字段

**Files:**
- Modify: `webui/src/composables/useRunSocket.ts`

- [ ] **Step 1: 给 per-run store 加字段**

在 `useRunSocket.ts` 的 `getStore` 里,`reactive({...})` 的初始对象(当前含 `transcript/nodes/edges/status/tokensUsed/error`)增加两个字段。把:

```typescript
    runStores.set(id, reactive({
      transcript: [] as { role: string; content: string }[],
      nodes: [] as any[], edges: [] as any[],
      status: 'idle', tokensUsed: 0, error: '',
    }))
```
替换为:
```typescript
    runStores.set(id, reactive({
      transcript: [] as { role: string; content: string }[],
      nodes: [] as any[], edges: [] as any[],
      status: 'idle', tokensUsed: 0, error: '',
      round: 0, lastCheckpoint: -1,
    }))
```

- [ ] **Step 2: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功(TS 会推断 reactive 形状;新增字段不破坏现有读取)。

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/composables/useRunSocket.ts
git commit -m "feat(webui): track round and lastCheckpoint in run store"
```

---

## Task 2: RunDashboard 组件

**Files:**
- Create: `webui/src/components/run/RunDashboard.vue`

- [ ] **Step 1: 写 RunDashboard**

创建 `webui/src/components/run/RunDashboard.vue`。它接收 props(status/tokensUsed/round/durationSec),渲染:阶段指示器(Graph→Task→Review→Done,高亮当前)、token 数、轮次、时长。阶段从 status 推断:`graph`/`Running`→Graph,`task_failed`/`task`→Task,`review`→Review,`Done`→Done。

```vue
<script setup lang="ts">
import { computed } from 'vue'

const props = defineProps<{
  status: string
  tokensUsed: number
  round: number
  durationSec: number
}>()

const PHASES = ['Graph', 'Task', 'Review', 'Done'] as const

// Map run status → active phase index.
const activeIdx = computed(() => {
  const s = (props.status || '').toLowerCase()
  if (s === 'done') return 3
  if (s.includes('review')) return 2
  if (s.includes('task')) return 1
  if (s === 'error' || s === 'cancelled') return -1
  return 0 // graph / running / paused / idle
})

function fmtTokens(n: number): string {
  if (n >= 1000) return (n / 1000).toFixed(1) + 'k'
  return String(n)
}
function fmtDuration(sec: number): string {
  if (sec < 60) return sec + 's'
  const m = Math.floor(sec / 60), s = sec % 60
  return `${m}m${s.toString().padStart(2, '0')}s`
}
</script>

<template>
  <div class="dashboard">
    <div class="phases">
      <template v-for="(p, i) in PHASES" :key="p">
        <span class="phase" :class="{ active: i === activeIdx, done: i < activeIdx }">{{ p }}</span>
        <span v-if="i < PHASES.length - 1" class="arrow" :class="{ done: i < activeIdx }">›</span>
      </template>
    </div>
    <div class="metrics">
      <span class="metric" title="轮次">⟳ {{ round }}</span>
      <span class="metric" title="Token 成本">◆ {{ fmtTokens(tokensUsed) }}</span>
      <span class="metric" title="时长">⏱ {{ fmtDuration(durationSec) }}</span>
    </div>
  </div>
</template>

<style scoped>
.dashboard {
  display: flex; align-items: center; justify-content: space-between;
  gap: 12px; padding: 5px 12px; border-bottom: 1px solid var(--border);
  background: var(--bg); font-size: 0.72rem;
}
.phases { display: flex; align-items: center; gap: 4px; }
.phase {
  padding: 2px 8px; border-radius: 10px; color: var(--text-muted);
  background: var(--bg-hover); font-weight: 500;
}
.phase.active { background: var(--accent); color: #fff; }
.phase.done { color: var(--success); background: var(--success-soft); }
.arrow { color: var(--text-muted); }
.arrow.done { color: var(--success); }
.metrics { display: flex; gap: 12px; color: var(--text-muted); font-family: var(--font-mono); }
.metric { white-space: nowrap; }
</style>
```

- [ ] **Step 2: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功。

- [ ] **Step 3: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/components/run/RunDashboard.vue
git commit -m "feat(webui): add RunDashboard (phase indicator + tokens + round + duration)"
```

---

## Task 3: RunView 挂载仪表盘 + 记录 round + 分支重跑按钮

**Files:**
- Modify: `webui/src/components/run/RunView.vue`

- [ ] **Step 1: 引入组件 + computed**

在 `<script setup>` import 区(`import DebugTimeline ...` 之后)加:
```typescript
import RunDashboard from './RunDashboard.vue'
```

现有有 computed `nodes`/`edges`/`status`/`errorMsg`(都从 `store.value` 取)。在它们附近加:
```typescript
const tokensUsed = computed(() => store.value?.tokensUsed || 0)
const round = computed(() => store.value?.round || 0)
const durationSec = computed(() => {
  const r = activeRunId.value ? findRun(activeRunId.value) : null
  return r?.duration_sec || 0
})
```
(`findRun` 已从 useRunSocket 导入;若未导入则在顶部 import 补上 `findRun`。)

- [ ] **Step 2: WS handler 记录 round / checkpoint**

在 `connectToRun` 的 `switch (e.type)` 里,现有 `case 'checkpoint':` 那行(把 checkpoint 推进 transcript 的)后面追加记录 round。把:
```typescript
      case 'checkpoint': s.transcript.push({ role: 'checkpoint', content: `📸 #${d.index} · r${d.round} · ${d.node_count}n/${d.edge_count}e` }); break
```
替换为:
```typescript
      case 'checkpoint':
        s.transcript.push({ role: 'checkpoint', content: `📸 #${d.index} · r${d.round} · ${d.node_count}n/${d.edge_count}e` })
        if (typeof d.round === 'number') s.round = d.round
        if (typeof d.index === 'number') s.lastCheckpoint = d.index
        break
```
另外,`status` 事件里也可能带阶段——现有 `case 'status':` 已更新 `s.status`,无需改。

- [ ] **Step 3: 加分支重跑函数**

在 `stopRun` 函数之后加 `branchRerun`(从最新 checkpoint 分支出一个新 run,并切过去):
```typescript
async function branchRerun() {
  const id = activeRunId.value
  if (!id) return
  const s = getRunStore(id)
  const fromCp = s && s.lastCheckpoint >= 0 ? s.lastCheckpoint : 0
  try {
    const resp = await fetch(`/api/runs/${id}/branch`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ from_checkpoint: fromCp }),
    }).then(r => r.json())
    if (resp?.id) {
      activeRunId.value = resp.id
      const ns = getRunStore(resp.id)
      ns.status = 'Running'
      connectToRun(resp.id)
    }
  } catch (e: any) {
    if (s) s.error = '分支重跑失败: ' + String(e)
  }
}
```

- [ ] **Step 4: 模板挂载仪表盘 + 按钮**

在 template 的 `.graph-stage` 内,`.stage-tabs` div **之后、图面板 template 之前**,插入仪表盘:
```html
      <RunDashboard v-if="activeRunId" :status="status" :tokensUsed="tokensUsed" :round="round" :durationSec="durationSec" />
```

在 `.chat-panel` 里的 `.toolbar` 内,现有停止按钮旁加分支重跑按钮。把:
```html
      <div class="toolbar">
        <button v-if="status === 'Running'" class="danger" @click="stopRun">{{ t('run.stop') }}</button>
        <span class="run-label" v-if="activeRunId">{{ activeRunId.slice(0,8) }}… · {{ status }}</span>
      </div>
```
替换为:
```html
      <div class="toolbar">
        <button v-if="status === 'Running' || status === 'graph'" class="danger" @click="stopRun">{{ t('run.stop') }}</button>
        <button v-if="activeRunId && (status === 'Done' || status === 'Error' || status === 'Cancelled' || status === 'paused')" class="secondary" @click="branchRerun">⑂ 分支重跑</button>
        <span class="run-label" v-if="activeRunId">{{ activeRunId.slice(0,8) }}… · {{ status }}</span>
      </div>
```

- [ ] **Step 5: 构建验证**

Run: `cd /home/hhhh/Graph-Centric/webui && npm run build`
Expected: 成功,无 TS 错误。

- [ ] **Step 6: 提交**

```bash
cd /home/hhhh/Graph-Centric
git add webui/src/components/run/RunView.vue
git commit -m "feat(webui): mount run dashboard, track round, add branch-rerun control"
```

---

## Task 4: 重建 dist + 重启 + 推送

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

## 验收(P3b 整体)
- `cd webui && npm run build` 通过,无类型错误。
- 运行页图区顶部出现仪表盘:阶段指示器(Graph→Task→Review→Done,当前高亮、已过阶段标绿)、轮次、token、时长。
- 跑任务时轮次随 checkpoint 事件递增、token 实时累加、阶段随 status 变化。
- 运行中显示「停止」;终态(Done/Error/Cancelled/paused)显示「⑂ 分支重跑」,点击从最新检查点创建新 run 并切过去。
- 深浅主题切换时仪表盘配色跟随。
- 现有功能无回归(对话、图、2D/3D 切换、分隔条)。

## 不做(超出范围 / YAGNI)
- 暂停运行中的 run:后端无端点,需新增 Rust(spec 已标注推迟);stop + branch 已覆盖主要控制。
- 手动拖拽编辑图节点:spec 明确留待后续。
- 完整 checkpoint 分支树可视化(CheckpointTree 空壳):本期分支重跑用最新 checkpoint,不做树形 UI。
