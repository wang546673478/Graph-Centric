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
