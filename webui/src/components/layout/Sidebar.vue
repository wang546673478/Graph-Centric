<script setup lang="ts">
import { inject } from 'vue'
import { activeRunId, runs, loadRuns } from '../../composables/useRunSocket'
import { onMounted } from 'vue'

onMounted(() => { loadRuns(); setInterval(loadRuns, 5000) })

function selectRun(id: string) { activeRunId.value = id }
</script>

<template>
  <aside class="sidebar">
    <div class="brand" @click="$router.push('/')">🔷 Graph-Centric</div>
    <div class="section-header">Runs</div>
    <div class="run-list">
      <div v-if="!runs.length" class="empty">No runs</div>
      <div
        v-for="r in runs" :key="r.id"
        class="run-item"
        :class="{ active: activeRunId === r.id }"
        @click="selectRun(r.id)"
      >
        <div class="task-line">{{ r.task?.slice(0, 50) || '(untitled)' }}</div>
        <div class="meta-line">{{ r.status }} · {{ r.duration_sec }}s</div>
      </div>
    </div>
  </aside>
</template>

<style scoped>
.sidebar {
  width: 220px; background: var(--bg-panel); border-right: 1px solid var(--border);
  display: flex; flex-direction: column; overflow-y: auto;
}
.brand { padding: 14px 12px; font-weight: 700; font-size: 0.95rem; cursor: pointer; color: var(--accent); }
.section-header { padding: 4px 12px; font-size: 0.65rem; text-transform: uppercase; color: var(--text-muted); letter-spacing: 0.5px; }
.empty { padding: 12px; color: var(--text-muted); font-size: 0.75rem; }
.run-item { padding: 6px 12px; cursor: pointer; border-radius: 0; }
.run-item:hover { background: var(--bg-hover); }
.run-item.active { background: var(--bg-hover); border-left: 3px solid var(--accent); }
.task-line { font-size: 0.78rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.meta-line { font-size: 0.65rem; color: var(--text-muted); margin-top: 1px; }
</style>
