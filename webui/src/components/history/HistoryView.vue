<script setup lang="ts">
import { onMounted } from 'vue'
import { useRouter } from 'vue-router'
import { runs, activeRunId, loadRuns, getRunStore, loadRunData } from '../../composables/useRunSocket'
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
</script>

<template>
  <div class="history">
    <h2>{{ t('history.title') }}</h2>
    <div v-if="!runs.length" class="empty">{{ t('history.empty') }}</div>
    <table v-else class="run-table">
      <thead><tr><th>ID</th><th>Task</th><th>Status</th><th>Duration</th></tr></thead>
      <tbody>
        <tr v-for="r in runs" :key="r.id" @click="openRun(r.id)" class="clickable">
          <td class="mono">{{ r.id.slice(0, 8) }}…</td>
          <td>{{ r.task?.slice(0, 60) || '(untitled)' }}</td>
          <td><span class="status-pill" :class="r.status.toLowerCase()">{{ r.status }}</span></td>
          <td>{{ r.duration_sec }}s</td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<style scoped>
.history { padding: 32px; max-width: 900px; }
h2 { font-size: 1.3rem; font-weight: 700; margin-bottom: 20px; }
.empty { color: var(--text-muted); padding: 24px 0; }
.run-table { width: 100%; border-collapse: collapse; }
.run-table th { text-align: left; padding: 8px 12px; font-size: 0.65rem; text-transform: uppercase; color: var(--text-muted); letter-spacing: 0.08em; border-bottom: 1px solid var(--border); }
.run-table td { padding: 10px 12px; font-size: 0.82rem; border-bottom: 1px solid var(--border); }
.mono { font-family: var(--font-mono); font-size: 0.75rem !important; color: var(--text-muted); }
.clickable { cursor: pointer; transition: background 0.1s; }
.clickable:hover { background: var(--bg); }
</style>
