<script setup lang="ts">
import { onMounted } from 'vue'
import { useRouter } from 'vue-router'
import { runs, activeRunId, loadRuns, getRunStore, loadRunData, api } from '../../composables/useRunSocket'
import { useI18n } from '../../composables/useI18n'
const { t } = useI18n()
const router = useRouter()

onMounted(() => { loadRuns() })

async function openRun(id: string) {
  activeRunId.value = id
  getRunStore(id)
  await loadRunData(id)
  router.push('/')
}

async function deleteRun(id: string, ev: Event) {
  ev.stopPropagation()  // don't trigger openRun
  try {
    await api.post(`/api/runs/${id}/delete`)
    if (activeRunId.value === id) activeRunId.value = null
    await loadRuns()
  } catch { /* ignore */ }
}

async function clearAll() {
  if (!confirm('确认清空所有运行记录?此操作不可撤销。')) return
  try {
    await api.del('/api/runs')
    activeRunId.value = null
    await loadRuns()
  } catch { /* ignore */ }
}
</script>

<template>
  <div class="history">
    <div class="head">
      <h2>{{ t('history.title') }}</h2>
      <button v-if="runs.length" class="danger clear-btn" @click="clearAll">🗑 清空全部</button>
    </div>
    <div v-if="!runs.length" class="empty">{{ t('history.empty') }}</div>
    <table v-else class="run-table">
      <thead><tr><th>ID</th><th>Task</th><th>Status</th><th>Duration</th><th></th></tr></thead>
      <tbody>
        <tr v-for="r in runs" :key="r.id" @click="openRun(r.id)" class="clickable">
          <td class="mono">{{ r.id.slice(0, 8) }}…</td>
          <td>{{ r.task?.slice(0, 60) || '(untitled)' }}</td>
          <td><span class="status-pill" :class="r.status.toLowerCase()">{{ r.status }}</span></td>
          <td>{{ r.duration_sec }}s</td>
          <td><button class="row-del" @click="deleteRun(r.id, $event)" title="删除此运行">🗑</button></td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<style scoped>
.history { padding: 32px; max-width: 900px; }
.head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 20px; }
h2 { font-size: 1.3rem; font-weight: 700; }
.clear-btn { font-size: 0.75rem; padding: 4px 12px; }
.empty { color: var(--text-muted); padding: 24px 0; }
.run-table { width: 100%; border-collapse: collapse; }
.run-table th { text-align: left; padding: 8px 12px; font-size: 0.65rem; text-transform: uppercase; color: var(--text-muted); letter-spacing: 0.08em; border-bottom: 1px solid var(--border); }
.run-table td { padding: 10px 12px; font-size: 0.82rem; border-bottom: 1px solid var(--border); }
.mono { font-family: var(--font-mono); font-size: 0.75rem !important; color: var(--text-muted); }
.clickable { cursor: pointer; transition: background 0.1s; }
.clickable:hover { background: var(--bg-hover); }
.row-del { background: none; border: none; cursor: pointer; font-size: 0.85rem; opacity: 0.5; padding: 2px 6px; border-radius: 4px; }
.row-del:hover { opacity: 1; background: var(--danger-soft); }
</style>
