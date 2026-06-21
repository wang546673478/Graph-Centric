<script setup lang="ts">
import { ref, computed, watch, onMounted, reactive } from 'vue'
import { activeRunId, runs, findRun, createRun, useRunSocket, detailMode, WSEvent, getRunStore, loadRunData } from '../../composables/useRunSocket'
import { useI18n } from '../../composables/useI18n'
import Transcript from './Transcript.vue'
import Composer from './Composer.vue'
import GraphPanel3D from '../graph/GraphPanel3D.vue'
import GraphPanel from '../graph/GraphPanel.vue'
import { useSplitter } from '../../composables/useSplitter'
import DebugTimeline from './DebugTimeline.vue'
import RunDashboard from './RunDashboard.vue'

const { t } = useI18n()
const tab = ref('graph')
const graphView = ref<'2d' | '3d'>('2d')
const { size: chatWidth, startDrag } = useSplitter(
  { storageKey: 'gc-chat-width', initial: 380, min: 280, max: 720 },
  true,
)
const graphFx = reactive<{ added: string[]; removed: string[]; replaced: boolean; ts: number }>(
  { added: [], removed: [], replaced: false, ts: 0 },
)
const clarifyOptions = ref<string[]>([])
const sending = ref(false)
let socket: ReturnType<typeof useRunSocket> | null = null

// Use global store or local fallback for active run.
const store = computed(() => activeRunId.value ? getRunStore(activeRunId.value) : null)

const transcript = computed(() => store.value?.transcript || [])
const nodes = computed(() => store.value?.nodes || [])
const edges = computed(() => store.value?.edges || [])
const status = computed(() => store.value?.status || 'idle')
const errorMsg = computed(() => store.value?.error || '')
const tokensUsed = computed(() => store.value?.tokensUsed || 0)
const round = computed(() => store.value?.round || 0)
const durationSec = computed(() => {
  const r = activeRunId.value ? findRun(activeRunId.value) : null
  return r?.duration_sec || 0
})

// Compute scope: nodes with edges in the graph are "in scope".
const scopeNodeIds = computed(() => {
  const ns = nodes.value
  const es = edges.value
  if (!ns.length || !es.length) return [] as string[]
  const connected = new Set<string>()
  for (const e of es) { connected.add(e.source); connected.add(e.target) }
  // If some nodes have no connections, the graph is likely single-node focus.
  if (connected.size === 0 && ns.length > 0) return ns.map(n => n.id)
  return [...connected]
})

// Connect WS for an active (non-terminal) run.
function connectToRun(id: string) {
  const s = getRunStore(id)
  if (!s) return
  if (socket) socket.disconnect()

  socket = useRunSocket(id, (e: WSEvent) => {
    const d = e.data || e
    switch (e.type) {
      case 'transcript': s.transcript.push({ role: d.role || 'assistant', content: d.content || '' }); break
      case 'graph': case 'graph_snapshot': if (d.nodes) s.nodes = d.nodes; if (d.edges) s.edges = d.edges; break
      case 'graph_patch':
        graphFx.added = d.added_nodes?.map((n: any) => n.id) || []
        graphFx.removed = d.removed_node_ids || []
        graphFx.replaced = !!d.replaced
        graphFx.ts = Date.now()
        break
      case 'status': if (d.phase) s.status = d.phase; s.tokensUsed = d.tokens_used || s.tokensUsed; break
      case 'loop_state':
        if (d.kind === 'Paused') { s.status = 'paused'; clarifyOptions.value = d.options || [] }
        else if (d.kind === 'GraphInvalid') s.status = 'graph_invalid'
        else if (d.kind === 'Done') s.status = 'Done'
        break
      case 'done': s.status = 'Done'; break
      case 'error': s.error = d.message || 'Unknown error'; s.status = 'Error'; break
      case 'cascade_step': s.transcript.push({ role: 'cascade', content: `🔍 ${d.changed_node} ← ${d.predecessor}: ${d.verdict} — ${d.rationale}` }); break
      case 'model_call': s.transcript.push({ role: 'model', content: `🤖 ${d.component} (${d.completion_tokens || 0}t, ${d.duration_ms || 0}ms): ${(d.response_content || '').slice(0, 200)}` }); break
      case 'checkpoint':
        s.transcript.push({ role: 'checkpoint', content: `📸 #${d.index} · r${d.round} · ${d.node_count}n/${d.edge_count}e` })
        if (typeof d.round === 'number') s.round = d.round
        if (typeof d.index === 'number') s.lastCheckpoint = d.index
        break
      case 'stream_chunk': {
        const comp = d.component || 'model'
        const streamRole = 'stream:' + comp
        const thinkRole = 'thinking:' + comp
        // Reasoning/thinking: per-component buffer.
        if (d.reasoning_content) {
          const thinkLast = s.transcript[s.transcript.length - 1]
          if (thinkLast && thinkLast.role === thinkRole) {
            thinkLast.content += d.reasoning_content
          } else {
            s.transcript.push({ role: thinkRole, content: d.reasoning_content })
          }
        }
        // Content: per-component buffer.
        if (d.content) {
          const last = s.transcript[s.transcript.length - 1]
          if (last && last.role === streamRole) {
            last.content += d.content
          } else {
            s.transcript.push({ role: streamRole, content: d.content })
          }
        }
        break
      }
      case 'stream_end': {
        const comp = d.component || 'model'
        const streamRole = 'stream:' + comp
        const thinkRole = 'thinking:' + comp
        // Lock streaming content to assistant.
        const last = s.transcript[s.transcript.length - 1]
        if (last && last.role === streamRole) {
          last.role = 'assistant'
        }
        // Lock thinking content.
        const thinkLast = s.transcript[s.transcript.length - 1]
        if (thinkLast && thinkLast.role === thinkRole) {
          thinkLast.role = 'thinking'
        }
        break
      }
    }
  })

  // Backfill: load checkpoint data for events that happened before WS connected.
  loadRunData(id)
}

// When activeRunId changes from sidebar, switch to that run.
watch(activeRunId, (id) => {
  if (id && getRunStore(id)) {
    connectToRun(id)
  }
})

function newChat() {
  if (socket) { socket.disconnect(); socket = null }
  activeRunId.value = null
}

async function stopRun() {
  const id = activeRunId.value
  if (!id) return
  try { await fetch(`/api/runs/${id}`, { method: 'DELETE' }) } catch { /* ignore */ }
  const s = getRunStore(id)
  if (s) s.status = 'Cancelled'
}

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

async function confirmStart() {
  clarifyOptions.value = []
  const id = activeRunId.value
  if (!id) return
  try {
    await fetch(`/api/runs/${id}/answer`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ answer: '__CONFIRM_START__' }),
    })
    const s = getRunStore(id)
    if (s) s.status = 'Running'
  } catch (e: any) {
    const s = getRunStore(id); if (s) s.error = String(e)
  }
}

async function submitTask(task: string) {
  clarifyOptions.value = []
  if (sending.value) return
  sending.value = true

  // If viewing a paused run, send the answer to resume it.
  const curId = activeRunId.value
  const curStore = curId ? getRunStore(curId) : null
  if (curId && curStore && curStore.status.toLowerCase() === 'paused') {
    try {
      await fetch(`/api/runs/${curId}/answer`, {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ answer: task }),
      })
      curStore.transcript.push({ role: 'user', content: task })
      curStore.status = 'Running'
    } catch (e: any) { curStore.error = String(e) }
    sending.value = false
    return
  }

  // Otherwise create a new run. If viewing a completed run, seed with its context.
  try {
    const body: any = { task }
    if (curId && curStore && curStore.transcript.length) {
      // Only send user + assistant messages as context (filter debug events).
      const ctx = curStore.transcript.filter(m => m.role === 'user' || m.role === 'assistant' || m.role === 'assistant_streaming' || m.role === 'thinking')
      if (ctx.length) body.initial_transcript = ctx.map(m => ({ role: m.role.replace('assistant_streaming', 'assistant').replace('thinking', 'assistant'), content: m.content }))
      if (curStore.nodes.length || curStore.edges.length) {
        body.initial_graph = { nodes: curStore.nodes, edges: curStore.edges }
      }
    }
    const resp = await fetch('/api/runs', {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    }).then(r => r.json())
    const id = resp.id
    activeRunId.value = id
    const s = getRunStore(id)!
    s.status = 'Running'; s.error = ''
    s.transcript = [{ role: 'user', content: task }]
    s.nodes = curStore?.nodes || []; s.edges = curStore?.edges || []; s.tokensUsed = 0
    connectToRun(id)
  } catch (e: any) {
    if (activeRunId.value) {
      const s = getRunStore(activeRunId.value)
      if (s) s.error = String(e)
    }
  } finally { sending.value = false }
}
</script>

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
      <RunDashboard v-if="activeRunId" :status="status" :tokensUsed="tokensUsed" :round="round" :durationSec="durationSec" />
      <template v-if="tab === 'graph'">
        <GraphPanel v-if="graphView === '2d'" :key="(activeRunId || 'empty') + '-2d'"
          :nodes="nodes" :edges="edges" :scopeNodeIds="scopeNodeIds" :fx="graphFx" />
        <GraphPanel3D v-else :key="(activeRunId || 'empty') + '-3d'"
          :nodes="nodes" :edges="edges" :scopeNodeIds="scopeNodeIds" :fx="graphFx" />
      </template>
      <DebugTimeline v-else-if="tab === 'debug'" />
    </div>

    <!-- 可拖拽分隔条 -->
    <div class="splitter" @mousedown="startDrag"></div>

    <!-- 对话右栏(可调宽) -->
    <div class="chat-panel" :style="{ width: chatWidth + 'px' }">
      <Transcript :messages="transcript" :status="status" :error="errorMsg" />
      <div class="toolbar">
        <button v-if="status === 'Running' || status === 'graph'" class="danger" @click="stopRun">{{ t('run.stop') }}</button>
        <button v-if="activeRunId && (status === 'Done' || status === 'Error' || status === 'Cancelled' || status === 'paused')" class="secondary" @click="branchRerun">⑂ 分支重跑</button>
        <span class="run-label" v-if="activeRunId">{{ activeRunId.slice(0,8) }}… · {{ status }}</span>
      </div>
      <div v-if="clarifyOptions.length" class="clarify-options">
        <button v-for="(opt, i) in clarifyOptions" :key="i" class="clarify-opt" @click="submitTask(opt)">
          {{ opt }}
        </button>
      </div>
      <Composer :disabled="sending" :paused="status === 'paused' || status === 'Paused'" @send="submitTask" @confirmStart="confirmStart" />
    </div>
  </div>
</template>

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
.clarify-options { display: flex; flex-wrap: wrap; gap: 6px; padding: 8px 12px 0; }
.clarify-opt {
  background: var(--bg-hover); border: 1px solid var(--border); color: var(--text);
  padding: 6px 12px; border-radius: 14px; font-size: 0.8rem; cursor: pointer;
}
.clarify-opt:hover { border-color: var(--accent); color: var(--accent); }
</style>
