<script setup lang="ts">
import { onMounted } from 'vue'
import { useRouter } from 'vue-router'
import { activeRunId, runs, loadRuns, getRunStore, loadRunData } from '../../composables/useRunSocket'
import { useI18n } from '../../composables/useI18n'

const { t } = useI18n()
const router = useRouter()

onMounted(() => { loadRuns(); setInterval(loadRuns, 5000) })

async function selectRun(id: string) {
  activeRunId.value = id
  getRunStore(id)
  await loadRunData(id)
  router.push('/')
}

function statusClass(s: string) {
  const m: Record<string, string> = { running: 's-running', paused: 's-paused', done: 's-done', error: 's-error' }
  return m[s] || ''
}
</script>

<template>
  <aside class="sidebar">
    <div class="brand" @click="router.push('/')">🔷 {{ t('brand') }}</div>
    <div class="section-header">{{ t('sidebar.runs') }}</div>
    <div class="run-list">
      <div v-if="!runs.length" class="empty">{{ t('sidebar.noRuns') }}</div>
      <div v-for="r in runs" :key="r.id" class="run-item" :class="{ active: activeRunId === r.id }" @click="selectRun(r.id)">
        <div class="task-line">{{ r.task?.slice(0, 50) || '(untitled)' }}</div>
        <div class="meta-line">
          <span :class="statusClass(r.status)">{{ r.status }}</span> · {{ r.duration_sec }}s
        </div>
      </div>
    </div>
  </aside>
</template>

<style scoped>
.sidebar {
  width: 220px; background: var(--bg-panel); border-right: 1px solid var(--border);
  display: flex; flex-direction: column; overflow-y: auto; box-shadow: var(--shadow); z-index: 1;
}
.brand { padding: 16px 14px; font-weight: 700; font-size: 0.95rem; cursor: pointer; color: var(--accent); letter-spacing: -0.01em; }
.section-header { padding: 6px 14px 4px; font-size: 0.6rem; text-transform: uppercase; color: var(--text-muted); letter-spacing: 0.08em; font-weight: 600; }
.empty { padding: 14px; color: var(--text-muted); font-size: 0.75rem; }
.run-item { padding: 8px 14px; cursor: pointer; border-left: 3px solid transparent; transition: all 0.1s ease; }
.run-item:hover { background: var(--bg); }
.run-item.active { background: var(--accent-soft); border-left-color: var(--accent); }
.task-line { font-size: 0.78rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.meta-line { font-size: 0.65rem; color: var(--text-muted); margin-top: 2px; }
.s-running { color: var(--accent); font-weight: 500; }
.s-paused { color: var(--warning); }
.s-done { color: var(--success); }
.s-error { color: var(--danger); }
</style>
