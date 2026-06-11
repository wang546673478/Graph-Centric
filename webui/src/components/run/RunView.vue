<script setup lang="ts">
import { ref, computed, watch, onMounted } from 'vue'
import { activeRunId, runs, findRun, createRun, useRunSocket, detailMode, WSEvent, getRunStore } from '../../composables/useRunSocket'
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
    switch (e.type) {
      case 'transcript': s.transcript.push({ role: e.role || 'assistant', content: e.content || '' }); break
      case 'graph': case 'graph_snapshot': if (e.nodes) s.nodes = e.nodes; if (e.edges) s.edges = e.edges; break
      case 'status': if (e.phase) s.status = e.phase; s.tokensUsed = e.tokens_used || s.tokensUsed; break
      case 'done': s.status = 'Done'; break
      case 'error': s.error = e.message || 'Unknown error'; s.status = 'Error'; break
      case 'cascade_step': if (detailMode.value) s.transcript.push({ role: 'cascade', content: `🔍 ${e.changed_node} ← ${e.predecessor}: ${e.verdict} — ${e.rationale}` }); break
      case 'model_call': if (detailMode.value) s.transcript.push({ role: 'model', content: `🤖 ${e.component} (${e.completion_tokens || 0}t, ${e.duration_ms || 0}ms): ${(e.response_content || '').slice(0, 200)}` }); break
      case 'checkpoint': s.transcript.push({ role: 'checkpoint', content: `📸 #${e.index} · r${e.round} · ${e.node_count}n/${e.edge_count}e` }); break
    }
  })
}

// When activeRunId changes from sidebar, switch to that run.
watch(activeRunId, (id) => {
  if (id && getRunStore(id)) {
    connectToRun(id)
  }
})

async function submitTask(task: string) {
  if (sending.value) return
  sending.value = true
  try {
    const { id } = await createRun(task)
    activeRunId.value = id
    // Initialize store for this new run.
    getRunStore(id)
    const s = getRunStore(id)!
    s.status = 'Running'; s.error = ''
    s.transcript = [{ role: 'user', content: task }]
    s.nodes = []; s.edges = []; s.tokensUsed = 0
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
      <Composer :disabled="sending || status === 'Running'" @send="submitTask" />
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
.side-panel { width: 420px; border-left: 1px solid var(--border); display: flex; flex-direction: column; background: var(--bg-panel); }
.tabs { display: flex; border-bottom: 1px solid var(--border); }
.tabs button { flex: 1; padding: 8px; background: none; color: var(--text-muted); border-radius: 0; font-size: 0.8rem; }
.tabs button.active { color: var(--accent); border-bottom: 2px solid var(--accent); font-weight: 500; }
.placeholder { padding: 24px; color: var(--text-muted); font-size: 0.85rem; }
</style>
