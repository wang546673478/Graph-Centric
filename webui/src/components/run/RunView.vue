<script setup lang="ts">
import { ref, computed } from 'vue'
import { activeRunId, runs, findRun, createRun, useRunSocket, detailMode, toggleDetailMode, WSEvent } from '../../composables/useRunSocket'
import Transcript from './Transcript.vue'
import Composer from './Composer.vue'
import GraphPanel from '../graph/GraphPanel.vue'

const tab = ref('graph')
const transcript = ref<{ role: string; content: string }[]>([])
const nodes = ref<any[]>([])
const edges = ref<any[]>([])
const status = ref('idle')
const tokensUsed = ref(0)
const statusMsg = ref('')
const errorMsg = ref('')
const sending = ref(false)
let socket: ReturnType<typeof useRunSocket> | null = null

const run = computed(() => activeRunId.value ? findRun(activeRunId.value) : null)

function handleEvent(e: WSEvent) {
  switch (e.type) {
    case 'transcript':
      transcript.value.push({ role: e.role || 'assistant', content: e.content || '' })
      break
    case 'graph':
    case 'graph_snapshot':
      if (e.nodes) nodes.value = e.nodes
      if (e.edges) edges.value = e.edges
      break
    case 'status':
      if (e.phase) status.value = e.phase
      if (e.message) statusMsg.value = e.message
      if (e.tokens_used) tokensUsed.value = e.tokens_used
      break
    case 'done':
      status.value = 'Done'
      break
    case 'error':
      errorMsg.value = e.message || 'Unknown error'
      status.value = 'Error'
      break
    case 'cascade_step':
      if (detailMode.value) {
        transcript.value.push({ role: 'cascade', content: `🔍 ${e.changed_node} ← ${e.predecessor}: ${e.verdict} — ${e.rationale}` })
      }
      break
    case 'model_call':
      if (detailMode.value) {
        transcript.value.push({ role: 'model', content: `🤖 ${e.component} (${e.completion_tokens || 0} tokens, ${e.duration_ms || 0}ms): ${(e.response_content || '').slice(0, 200)}` })
      }
      break
    case 'checkpoint':
      transcript.value.push({ role: 'checkpoint', content: `📸 checkpoint #${e.index} · round ${e.round} · ${e.node_count}n/${e.edge_count}e` })
      break
  }
}

async function submitTask(task: string) {
  if (sending.value) return
  sending.value = true
  errorMsg.value = ''
  try {
    const { id } = await createRun(task)
    activeRunId.value = id
    status.value = 'Running'
    transcript.value = [{ role: 'user', content: task }]
    nodes.value = []
    edges.value = []
    tokensUsed.value = 0
    if (socket) socket.disconnect()
    socket = useRunSocket(id, handleEvent)
  } catch (e: any) {
    errorMsg.value = String(e)
  } finally {
    sending.value = false
  }
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
        <button :class="{ active: tab === 'graph' }" @click="tab = 'graph'">Graph</button>
        <button :class="{ active: tab === 'files' }" @click="tab = 'files'">Files</button>
      </div>
      <GraphPanel v-if="tab === 'graph'" :nodes="nodes" :edges="edges" />
      <div v-else class="placeholder">Files view — coming soon</div>
    </div>
  </div>
</template>

<style scoped>
.run-view { display: flex; flex: 1; min-height: 0; }
.chat-panel { flex: 1; display: flex; flex-direction: column; min-width: 0; }
.side-panel { width: 420px; border-left: 1px solid var(--border); display: flex; flex-direction: column; }
.tabs { display: flex; border-bottom: 1px solid var(--border); }
.tabs button { flex: 1; padding: 6px; background: none; color: var(--text-muted); border-radius: 0; font-size: 0.8rem; }
.tabs button.active { color: var(--text); border-bottom: 2px solid var(--accent); }
.placeholder { padding: 24px; color: var(--text-muted); font-size: 0.85rem; }
</style>
