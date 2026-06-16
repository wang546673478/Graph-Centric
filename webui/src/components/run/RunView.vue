<script setup lang="ts">
import { ref, computed, watch, onMounted } from 'vue'
import { activeRunId, runs, findRun, createRun, useRunSocket, detailMode, WSEvent, getRunStore, loadRunData } from '../../composables/useRunSocket'
import { useI18n } from '../../composables/useI18n'
import Transcript from './Transcript.vue'
import Composer from './Composer.vue'
import GraphPanel from '../graph/GraphPanel.vue'

const { t } = useI18n()
const tab = ref('graph')
const sending = ref(false)
let socket: ReturnType<typeof useRunSocket> | null = null

// Use global store or local fallback for active run.
const store = computed(() => activeRunId.value ? getRunStore(activeRunId.value) : null)

const transcript = computed(() => store.value?.transcript || [])
const nodes = computed(() => store.value?.nodes || [])
const edges = computed(() => store.value?.edges || [])
const status = computed(() => store.value?.status || 'idle')
const errorMsg = computed(() => store.value?.error || '')

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
      case 'status': if (d.phase) s.status = d.phase; s.tokensUsed = d.tokens_used || s.tokensUsed; break
      case 'done': s.status = 'Done'; break
      case 'error': s.error = d.message || 'Unknown error'; s.status = 'Error'; break
      case 'cascade_step': if (detailMode.value) s.transcript.push({ role: 'cascade', content: `🔍 ${d.changed_node} ← ${d.predecessor}: ${d.verdict} — ${d.rationale}` }); break
      case 'model_call': if (detailMode.value) s.transcript.push({ role: 'model', content: `🤖 ${d.component} (${d.completion_tokens || 0}t, ${d.duration_ms || 0}ms): ${(d.response_content || '').slice(0, 200)}` }); break
      case 'checkpoint': s.transcript.push({ role: 'checkpoint', content: `📸 #${d.index} · r${d.round} · ${d.node_count}n/${d.edge_count}e` }); break
      case 'stream_chunk': {
        const last = s.transcript[s.transcript.length - 1]
        if (last && last.role === 'assistant_streaming') {
          last.content += d.content || ''
        } else {
          s.transcript.push({ role: 'assistant_streaming', content: d.content || '' })
        }
        break
      }
      case 'stream_end': {
        const last = s.transcript[s.transcript.length - 1]
        if (last && last.role === 'assistant_streaming') {
          last.role = 'assistant'
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

async function submitTask(task: string) {
  if (sending.value) return
  sending.value = true

  // If viewing a paused run, send the answer to resume it.
  const curId = activeRunId.value
  const curStore = curId ? getRunStore(curId) : null
  if (curId && curStore && curStore.status === 'Paused') {
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
      body.initial_transcript = curStore.transcript.map(m => ({ role: m.role, content: m.content }))
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
    <div class="chat-panel">
      <Transcript :messages="transcript" :status="status" :error="errorMsg" />
      <div class="toolbar">
        <button v-if="status === 'Running'" class="danger" @click="stopRun">{{ t('run.stop') }}</button>
        <span class="run-label" v-if="activeRunId">{{ activeRunId.slice(0,8) }}… · {{ status }}</span>
      </div>
      <Composer :disabled="sending" @send="submitTask" />
    </div>
    <div class="side-panel">
      <div class="tabs">
        <button :class="{ active: tab === 'graph' }" @click="tab = 'graph'">{{ t('graph.tab') }}</button>
        <button :class="{ active: tab === 'files' }" @click="tab = 'files'">{{ t('graph.filesTab') }}</button>
      </div>
      <GraphPanel v-if="tab === 'graph'" :nodes="nodes" :edges="edges" />
      <div v-else class="placeholder">{{ t('files.empty') }}</div>
    </div>
  </div>
</template>

<style scoped>
.run-view { display: flex; flex: 1; min-height: 0; }
.chat-panel { flex: 1; display: flex; flex-direction: column; min-width: 0; }
.toolbar { display: flex; align-items: center; gap: 8px; padding: 4px 12px; border-top: 1px solid var(--border); background: var(--bg); }
.toolbar button { font-size: 0.75rem; padding: 4px 10px; }
.run-label { font-size: 0.7rem; color: var(--text-muted); font-family: var(--font-mono); }
.side-panel { width: 420px; border-left: 1px solid var(--border); display: flex; flex-direction: column; background: var(--bg-panel); }
.tabs { display: flex; border-bottom: 1px solid var(--border); }
.tabs button { flex: 1; padding: 8px; background: none; color: var(--text-muted); border-radius: 0; font-size: 0.8rem; }
.tabs button.active { color: var(--accent); border-bottom: 2px solid var(--accent); font-weight: 500; }
.placeholder { padding: 24px; color: var(--text-muted); font-size: 0.85rem; }
</style>
