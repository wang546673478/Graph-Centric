<script setup lang="ts">
import { onMounted, ref, computed } from 'vue'
import { useRouter } from 'vue-router'
import { activeRunId, runs, loadRuns, getRunStore, loadRunData } from '../../composables/useRunSocket'
import { useI18n } from '../../composables/useI18n'

const { t } = useI18n()
const router = useRouter()

onMounted(() => { loadRuns(); setInterval(loadRuns, 5000) })

const search = ref('')
const statusFilter = ref<'all' | 'running' | 'paused' | 'done' | 'error'>('all')

const filtered = computed(() => {
  const q = search.value.trim().toLowerCase()
  return runs.value.filter(r => {
    if (statusFilter.value !== 'all') {
      const s = (r.status || '').toLowerCase()
      const k = statusFilter.value
      if (k === 'running' && !s.includes('running') && !s.includes('graph')) return false
      if (k === 'paused' && !s.includes('paused')) return false
      if (k === 'done' && !s.includes('done')) return false
      if (k === 'error' && !s.includes('error')) return false
    }
    if (q) return (r.task || '').toLowerCase().includes(q)
    return true
  })
})

async function selectRun(id: string) {
  activeRunId.value = id
  getRunStore(id)
  await loadRunData(id)
  router.push('/')
}

function quickStart() {
  // New-chat shortcut: jump to a fresh composer and focus the input.
  activeRunId.value = null
  router.push('/')
  setTimeout(() => {
    const el = document.querySelector('.composer input') as HTMLInputElement | null
    el?.focus()
  }, 50)
}

function statusClass(s: string) {
  const m: Record<string, string> = { running: 's-running', paused: 's-paused', done: 's-done', error: 's-error' }
  return m[s] || ''
}
</script>

<template>
  <aside class="sidebar">
    <div class="brand" @click="router.push('/')">🔷 {{ t('brand') }}</div>
    <div class="toolbar">
      <button class="new-run" @click="quickStart" title="新建任务 (按 / 聚焦 composer)">＋ 新建</button>
      <input v-model="search" class="search" placeholder="搜索 task…" title="搜索（按 / 聚焦）" />
      <select v-model="statusFilter" class="filter" title="按状态过滤">
        <option value="all">全部</option>
        <option value="running">运行中</option>
        <option value="paused">已暂停</option>
        <option value="done">已完成</option>
        <option value="error">出错</option>
      </select>
    </div>
    <div class="section-header">
      <span>{{ t('sidebar.runs') }}</span>
      <span class="count">{{ filtered.length }} / {{ runs.length }}</span>
    </div>
    <div class="run-list" v-if="runs.length">
      <div v-if="!filtered.length" class="empty small">
        <div class="empty-icon">🔍</div>
        <div class="empty-msg">没有匹配的 run</div>
        <button class="empty-cta-secondary" @click="search = ''; statusFilter = 'all'">清除过滤</button>
      </div>
      <div v-for="r in filtered" :key="r.id" class="run-item" :class="{ active: activeRunId === r.id }" @click="selectRun(r.id)">
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
  width: 240px; background: var(--bg-panel); border-right: 1px solid var(--border);
  display: flex; flex-direction: column; overflow: hidden; box-shadow: var(--shadow); z-index: 1;
}
.brand { padding: 14px 14px 8px; font-weight: 700; font-size: 0.95rem; cursor: pointer; color: var(--accent); letter-spacing: -0.01em; }
.toolbar { padding: 4px 10px 8px; display: flex; flex-direction: column; gap: 4px; border-bottom: 1px solid var(--border); }
.new-run { padding: 6px 10px; background: var(--accent); color: #fff; border: none; border-radius: 4px; cursor: pointer; font-size: 0.8rem; font-weight: 500; transition: filter 0.1s; }
.new-run:hover { filter: brightness(1.1); }
.search { width: 100%; padding: 5px 8px; font-size: 0.75rem; border: 1px solid var(--border); background: var(--bg); color: var(--text); border-radius: 4px; box-sizing: border-box; }
.search:focus { outline: none; border-color: var(--accent); }
.filter { width: 100%; padding: 4px 6px; font-size: 0.7rem; border: 1px solid var(--border); background: var(--bg); color: var(--text); border-radius: 4px; box-sizing: border-box; }
.section-header { padding: 8px 14px 4px; font-size: 0.6rem; text-transform: uppercase; color: var(--text-muted); letter-spacing: 0.08em; font-weight: 600; display: flex; justify-content: space-between; align-items: baseline; }
.count { font-size: 0.65rem; color: var(--text-muted); font-weight: 400; text-transform: none; letter-spacing: 0; }
.empty { padding: 20px 14px; color: var(--text-muted); text-align: center; }
.empty.small { padding: 12px 8px; }
.empty-icon { font-size: 1.5rem; margin-bottom: 6px; opacity: 0.6; }
.empty-msg { font-size: 0.75rem; margin-bottom: 10px; }
.empty-cta, .empty-cta-secondary { padding: 6px 12px; border-radius: 4px; font-size: 0.72rem; cursor: pointer; border: none; transition: filter 0.1s; }
.empty-cta { background: var(--accent); color: #fff; }
.empty-cta:hover, .empty-cta-secondary:hover { filter: brightness(1.1); }
.empty-cta-secondary { background: var(--bg); color: var(--text-muted); border: 1px solid var(--border); }
.run-list { flex: 1; overflow-y: auto; }
.run-item { padding: 8px 14px; cursor: pointer; border-left: 3px solid transparent; transition: background 0.1s ease; }
.run-item:hover { background: var(--bg); }
.run-item.active { background: var(--accent-soft); border-left-color: var(--accent); }
.task-line { font-size: 0.78rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.meta-line { font-size: 0.7rem; color: var(--text-muted); margin-top: 2px; }
.s-running { color: var(--accent); font-weight: 500; }
.s-paused { color: var(--warning); }
.s-done { color: var(--success); }
.s-error { color: var(--danger); }
</style>
