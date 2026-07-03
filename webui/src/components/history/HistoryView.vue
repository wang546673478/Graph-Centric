<script setup lang="ts">
import { onMounted, ref, computed } from 'vue'
import { useRouter } from 'vue-router'
import { runs, activeRunId, loadRuns, getRunStore, loadRunData, api } from '../../composables/useRunSocket'
import { useI18n } from '../../composables/useI18n'
const { t } = useI18n()
const router = useRouter()

onMounted(() => { loadRuns() })

// Layout fix: search + status filter + pagination. Without these,
// the page was a long unbounded table — hard to find a specific
// run when the system has accumulated 20+ runs.
const search = ref('')
const statusFilter = ref<'all' | 'done' | 'error' | 'paused' | 'running'>('all')
const PAGE_SIZE = 30
const showAll = ref(false)

const filtered = computed(() => {
  const q = search.value.trim().toLowerCase()
  return runs.value.filter(r => {
    if (statusFilter.value !== 'all') {
      const s = (r.status || '').toLowerCase()
      if (!s.includes(statusFilter.value)) return false
    }
    if (q) return (r.task || '').toLowerCase().includes(q) || r.id.toLowerCase().includes(q)
    return true
  })
})
const visible = computed(() => {
  if (showAll.value) return filtered.value
  return filtered.value.slice(0, PAGE_SIZE)
})
const hiddenCount = computed(() => filtered.value.length - visible.value.length)

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

// Map a raw status string to a CSS class. The backend returns variants
// like 'Done', 'Error', 'Paused', 'Running' (camel-case) plus object
// forms like '{Done: null}'. Normalize to lowercase letters.
function statusKey(s: string) {
  const m = (s || '').toLowerCase()
  if (m.includes('error')) return 'error'
  if (m.includes('paused')) return 'paused'
  if (m.includes('running')) return 'running'
  if (m.includes('done')) return 'done'
  return 'unknown'
}
</script>

<template>
  <div class="history">
    <div class="head">
      <h2>{{ t('history.title') }}</h2>
      <button v-if="runs.length" class="danger clear-btn" @click="clearAll">🗑 清空全部</button>
    </div>

    <!-- Layout fix: filter row was missing — users had to scroll
         through all 24+ runs to find one. Now: search box +
         status dropdown + match-count badge. -->
    <div v-if="runs.length" class="filters">
      <input v-model="search" class="search" placeholder="搜索 task 或 run id…" />
      <select v-model="statusFilter" class="filter">
        <option value="all">全部 ({{ runs.length }})</option>
        <option value="done">已完成</option>
        <option value="error">出错</option>
        <option value="paused">已暂停</option>
        <option value="running">运行中</option>
      </select>
      <span class="match-count">{{ filtered.length }} / {{ runs.length }}</span>
    </div>

    <div v-if="!runs.length" class="empty">{{ t('history.empty') }}</div>
    <div v-else-if="!filtered.length" class="empty">
      没有匹配的 run
      <button class="empty-cta-secondary" @click="search = ''; statusFilter = 'all'">清除过滤</button>
    </div>
    <table v-else class="run-table">
      <thead><tr><th>ID</th><th>Task</th><th>Status</th><th>Duration</th><th></th></tr></thead>
      <tbody>
        <tr v-for="r in visible" :key="r.id" @click="openRun(r.id)" class="clickable">
          <td class="mono">{{ r.id.slice(0, 8) }}…</td>
          <td>{{ r.task?.slice(0, 60) || '(untitled)' }}</td>
          <td><span class="status-pill" :class="statusKey(r.status)">{{ r.status }}</span></td>
          <td>{{ r.duration_sec }}s</td>
          <td><button class="row-del" @click="deleteRun(r.id, $event)" title="删除此运行">🗑</button></td>
        </tr>
      </tbody>
    </table>
    <div v-if="hiddenCount > 0" class="show-more">
      还有 {{ hiddenCount }} 个 run 未显示
      <button @click="showAll = true">显示全部</button>
    </div>
  </div>
</template>

<style scoped>
.history { padding: 32px; max-width: 900px; }
.head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 20px; }
h2 { font-size: 1.3rem; font-weight: 700; }
.clear-btn { font-size: 0.75rem; padding: 4px 12px; }
.empty { color: var(--text-muted); padding: 24px 0; display: flex; flex-direction: column; align-items: flex-start; gap: 8px; }
.empty-cta-secondary { padding: 4px 10px; border-radius: 4px; font-size: 0.7rem; cursor: pointer; background: var(--bg); color: var(--text-muted); border: 1px solid var(--border); }
.empty-cta-secondary:hover { color: var(--text); border-color: var(--accent); }
.filters { display: flex; gap: 8px; align-items: center; margin-bottom: 16px; }
.search { flex: 1; padding: 6px 10px; font-size: 0.8rem; border: 1px solid var(--border); background: var(--bg); color: var(--text); border-radius: 4px; }
.search:focus { outline: none; border-color: var(--accent); }
.filter { padding: 6px 10px; font-size: 0.8rem; border: 1px solid var(--border); background: var(--bg); color: var(--text); border-radius: 4px; }
.match-count { font-size: 0.75rem; color: var(--text-muted); font-family: var(--font-mono); white-space: nowrap; }
.run-table { width: 100%; border-collapse: collapse; }
.run-table th { text-align: left; padding: 8px 12px; font-size: 0.65rem; text-transform: uppercase; color: var(--text-muted); letter-spacing: 0.08em; border-bottom: 1px solid var(--border); }
.run-table td { padding: 10px 12px; font-size: 0.82rem; border-bottom: 1px solid var(--border); }
.mono { font-family: var(--font-mono); font-size: 0.75rem !important; color: var(--text-muted); }
.clickable { cursor: pointer; transition: background 0.1s; }
.clickable:hover { background: var(--bg-hover); }
.row-del { background: none; border: none; cursor: pointer; font-size: 0.85rem; opacity: 0.5; padding: 2px 6px; border-radius: 4px; }
.row-del:hover { opacity: 1; background: var(--danger-soft); }
.status-pill { display: inline-block; padding: 1px 8px; border-radius: 10px; font-size: 0.7rem; font-weight: 500; }
.status-pill.done { background: var(--success-soft); color: var(--success); }
.status-pill.error { background: var(--danger-soft); color: var(--danger); }
.status-pill.paused { background: var(--warning-soft); color: var(--warning); }
.status-pill.running { background: var(--accent-soft); color: var(--accent); }
.status-pill.unknown { background: var(--bg-hover); color: var(--text-muted); }
.show-more { padding: 12px 0; text-align: center; font-size: 0.78rem; color: var(--text-muted); display: flex; gap: 10px; justify-content: center; align-items: center; }
.show-more button { padding: 4px 12px; background: var(--bg); color: var(--text-muted); border: 1px solid var(--border); border-radius: 4px; font-size: 0.75rem; cursor: pointer; }
.show-more button:hover { color: var(--text); border-color: var(--accent); }
</style>
