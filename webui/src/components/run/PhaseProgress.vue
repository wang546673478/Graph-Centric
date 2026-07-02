<!--
  PhaseProgress — v2 spec §4.2.

  Top status bar showing the agent's current graph phase + key
  counters. Reads from the per-run store's `phaseProgress` field,
  which is fed by the `graph_phase` WebSocket event from
  GraphLoop::emit_graph_phase().
-->
<template>
  <div v-if="progress" class="phase-progress">
    <div class="phase-pill" :class="`phase-${phaseClass}`">
      <span class="phase-label">{{ phaseLabel }}</span>
    </div>
    <div class="phase-meta">
      <span class="meta-item">round <b>{{ progress.round }}</b></span>
      <span v-if="progress.graph_version != null" class="meta-item">v<b>{{ progress.graph_version }}</b></span>
      <span v-if="progress.clarification_count > 0" class="meta-item warn">
        澄清 <b>{{ progress.clarification_count }}</b> 轮
      </span>
      <span v-if="progress.explorer_iter > 0" class="meta-item" :class="explorerClass">
        探索 <b>{{ progress.explorer_iter }}</b> 轮
      </span>
    </div>
  </div>
  <div v-else class="phase-progress phase-empty">
    <div class="phase-pill phase-clarifying">
      <span class="phase-label">等待</span>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'

const props = defineProps<{
  progress: {
    graph_phase: string
    round: number
    clarification_count: number
    explorer_iter: number
    graph_version: number
    ts: number
  } | null
}>()

const phaseClass = computed(() => {
  switch (props.progress?.graph_phase) {
    case 'clarifying': return 'clarifying'
    case 'seeding': return 'seeding'
    case 'filling': return 'filling'
    case 'expanding': return 'expanding'
    case 'verifying': return 'verifying'
    default: return 'clarifying'
  }
})

const phaseLabel = computed(() => {
  switch (props.progress?.graph_phase) {
    case 'clarifying': return '澄清中'
    case 'seeding': return '建种子'
    case 'filling': return '填充中'
    case 'expanding': return '展开中'
    case 'verifying': return '验收中'
    default: return '等待中'
  }
})

const explorerClass = computed(() => {
  const n = props.progress?.explorer_iter || 0
  if (n >= 150) return 'danger'
  if (n >= 100) return 'warn'
  return ''
})
</script>

<style scoped>
.phase-progress {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 6px 12px;
  background: var(--bg-secondary, #f5f5f5);
  border-radius: 6px;
  font-size: 13px;
}
.phase-pill {
  padding: 2px 10px;
  border-radius: 12px;
  font-weight: 600;
  color: white;
}
.phase-clarifying { background: #3b82f6; }
.phase-seeding    { background: #6366f1; }
.phase-filling    { background: #10b981; }
.phase-expanding  { background: #8b5cf6; }
.phase-verifying  { background: #f59e0b; }
.phase-meta { display: flex; gap: 12px; }
.meta-item { color: var(--text-secondary, #666); }
.meta-item b { color: var(--text-primary, #000); }
.meta-item.warn { color: #f59e0b; }
.meta-item.danger { color: #ef4444; }
.phase-empty { opacity: 0.6; }
</style>
