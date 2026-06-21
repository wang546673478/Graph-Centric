<script setup lang="ts">
import { computed, ref } from 'vue'
import { getRunStore } from '../../composables/useRunSocket'
import { activeRunId } from '../../composables/useRunSocket'

const expanded = ref<Set<number>>(new Set())

const store = computed(() => activeRunId.value ? getRunStore(activeRunId.value) : null)

interface TimelineItem {
  type: string
  round: number
  timestamp: number
  data: any
}

const timeline = computed<TimelineItem[]>(() => {
  const s = store.value
  if (!s) return []
  const items: TimelineItem[] = []
  const msgs = s.transcript
  for (let i = 0; i < msgs.length; i++) {
    const m = msgs[i]
    if (m.role === 'model') {
      items.push({ type: 'model_call', round: Math.floor(i / 3), timestamp: i, data: m })
    } else if (m.role === 'tool_result') {
      items.push({ type: 'tool_use', round: Math.floor(i / 3), timestamp: i, data: m })
    } else if (m.role === 'cascade') {
      items.push({ type: 'cascade_step', round: Math.floor(i / 3), timestamp: i, data: m })
    } else if (m.role === 'checkpoint') {
      items.push({ type: 'checkpoint', round: Math.floor(i / 3), timestamp: i, data: m })
    } else if (m.role !== 'user' && m.role !== 'assistant_streaming') {
      items.push({ type: 'transcript', round: Math.floor(i / 3), timestamp: i, data: m })
    }
  }
  return items
})

const groupedByRound = computed(() => {
  const groups: Map<number, TimelineItem[]> = new Map()
  for (const item of timeline.value) {
    if (!groups.has(item.round)) groups.set(item.round, [])
    groups.get(item.round)!.push(item)
  }
  return [...groups.entries()].sort((a, b) => a[0] - b[0])
})

function toggle(idx: number) {
  if (expanded.value.has(idx)) expanded.value.delete(idx)
  else expanded.value.add(idx)
  expanded.value = new Set(expanded.value)
}

const graph = computed(() => ({
  nodes: store.value?.nodes?.length || 0,
  edges: store.value?.edges?.length || 0,
}))
</script>

<template>
  <div class="debug-timeline">
    <div v-if="!timeline.length" class="empty">
      No trace data yet. Enable <b>Detail Mode</b> and run a task to see LLM calls, tool uses, and cascade steps.
    </div>
    <div v-for="[round, items] in groupedByRound" :key="round" class="round-group">
      <div class="round-header" @click="toggle(round)">
        <span class="arrow">{{ expanded.has(round) ? '▼' : '▶' }}</span>
        <span class="round-label">Round {{ round }}</span>
        <span class="item-count">{{ items.length }} event{{ items.length > 1 ? 's' : '' }}</span>
      </div>
      <div v-if="expanded.has(round)" class="round-body">
        <div v-for="(item, i) in items" :key="i" class="timeline-item" :class="item.type">
          <!-- Model Call -->
          <div v-if="item.type === 'model_call'" class="entry model">
            <div class="entry-header">
              <span class="badge model-badge">LLM</span>
              <span class="entry-role">{{ item.data.role }}</span>
            </div>
            <pre class="entry-body">{{ item.data.content?.slice(0, 2000) }}</pre>
          </div>

          <!-- Tool Use -->
          <div v-else-if="item.type === 'tool_use'" class="entry tool">
            <div class="entry-header">
              <span class="badge tool-badge">TOOL</span>
              <span class="entry-role">{{ item.data.role }}</span>
            </div>
            <pre class="entry-body">{{ item.data.content?.slice(0, 500) }}</pre>
          </div>

          <!-- Cascade Step -->
          <div v-else-if="item.type === 'cascade_step'" class="entry cascade">
            <div class="entry-header">
              <span class="badge cascade-badge">CASCADE</span>
              <span class="entry-role">{{ item.data.role }}</span>
            </div>
            <pre class="entry-body">{{ item.data.content?.slice(0, 500) }}</pre>
          </div>

          <!-- Checkpoint -->
          <div v-else-if="item.type === 'checkpoint'" class="entry checkpoint">
            <div class="entry-header">
              <span class="badge cp-badge">CP</span>
              <span class="entry-role">{{ item.data.role }}</span>
            </div>
            <pre class="entry-body">{{ item.data.content }}</pre>
          </div>

          <!-- Transcript -->
          <div v-else class="entry transcript">
            <div class="entry-header">
              <span class="badge msg-badge">{{ item.data.role }}</span>
            </div>
            <pre class="entry-body">{{ item.data.content?.slice(0, 300) }}</pre>
          </div>
        </div>
      </div>
    </div>
    <div class="footer-bar">
      Graph: {{ graph.nodes }}n / {{ graph.edges }}e · {{ timeline.length }} events
    </div>
  </div>
</template>

<style scoped>
.debug-timeline { flex: 1; overflow-y: auto; padding: 8px; }
.empty { color: var(--text-muted); font-size: 0.8rem; padding: 24px; text-align: center; }
.round-group { margin-bottom: 4px; }
.round-header {
  display: flex; align-items: center; gap: 6px; padding: 6px 8px;
  cursor: pointer; font-size: 0.78rem; font-weight: 500;
  color: var(--text); background: var(--bg); border-radius: var(--radius);
  user-select: none;
}
.round-header:hover { background: var(--bg-hover); }
.arrow { font-size: 0.6rem; width: 12px; color: var(--text-muted); }
.round-label { flex: 1; }
.item-count { font-size: 0.65rem; color: var(--text-muted); }
.round-body { padding: 2px 0 4px 16px; }
.timeline-item { border-left: 2px solid var(--border); margin-left: 4px; }
.entry { padding: 4px 10px; margin: 2px 0; border-radius: 4px; font-size: 0.72rem; }
.entry-header { display: flex; align-items: center; gap: 6px; margin-bottom: 2px; }
.badge { padding: 0 4px; border-radius: 3px; font-size: 0.58rem; font-weight: 600; text-transform: uppercase; letter-spacing: 0.03em; }
.model-badge { background: #ede9fe; color: #7c3aed; }
.tool-badge { background: #fef3c7; color: #d97706; }
.cascade-badge { background: #dbeafe; color: #2563eb; }
.cp-badge { background: #d1fae5; color: #059669; }
.msg-badge { background: var(--bg); color: var(--text-muted); }
.entry-role { font-size: 0.62rem; color: var(--text-muted); text-transform: uppercase; }
.entry-body { white-space: pre-wrap; word-break: break-word; font-size: 0.68rem; color: var(--text); margin: 0; line-height: 1.4; max-height: 200px; overflow-y: auto; }
.entry.model { background: #faf5ff; }
.entry.tool { background: #fffbeb; }
.entry.cascade { background: #eff6ff; }
.entry.checkpoint { background: #ecfdf5; }
.entry.transcript { background: var(--bg); }
.footer-bar { padding: 6px 8px; font-size: 0.65rem; color: var(--text-muted); border-top: 1px solid var(--border); }
</style>
