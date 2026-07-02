<!--
  SubRunTree — v2 spec §4.5.

  Tree panel showing the drill-down sub-runs of the current run.
  Each entry:
    - the parent complex node id
    - the sub-run id
    - the current status (running / done / error)
    - a "jump" link to the sub-run's own view

  Data source: GET /api/runs/:id/sub-runs.
-->
<template>
  <div v-if="entries.length" class="sub-run-tree">
    <div class="srt-header">
      <span>🔍 子 run</span>
      <span class="srt-count">{{ entries.length }} 个</span>
    </div>
    <ul>
      <li v-for="e in entries" :key="e.sub_run_id" class="srt-item">
        <span class="srt-node">{{ e.node_id }}</span>
        <span class="srt-arrow">→</span>
        <span class="srt-id">{{ e.sub_run_id.slice(0, 8) }}…</span>
        <span :class="['srt-status', `srt-${e.sub_status}`]">{{ statusLabel(e.sub_status) }}</span>
        <a class="srt-jump" :href="`/runs/${e.sub_run_id}`">查看</a>
      </li>
    </ul>
  </div>
</template>

<script setup lang="ts">
defineProps<{
  entries: { node_id: string; sub_run_id: string; sub_status: string }[]
}>()

function statusLabel(s: string): string {
  if (s === 'running') return '运行中'
  if (s === 'done') return '完成'
  if (s === 'error') return '错误'
  return s
}
</script>

<style scoped>
.sub-run-tree {
  background: var(--bg-secondary, #f5f5f5);
  border-radius: 6px;
  padding: 8px 12px;
  margin: 6px 0;
  font-size: 12px;
}
.srt-header { display: flex; justify-content: space-between; color: var(--text-secondary); }
.srt-count { color: var(--text-tertiary, #999); }
.sub-run-tree ul { list-style: none; padding: 0; margin: 6px 0 0 0; }
.srt-item { display: flex; align-items: center; gap: 6px; padding: 3px 0; }
.srt-node { font-family: monospace; color: var(--text-primary); }
.srt-arrow { color: var(--text-tertiary, #999); }
.srt-id { font-family: monospace; color: var(--text-secondary); }
.srt-status { padding: 1px 6px; border-radius: 3px; color: white; font-size: 11px; }
.srt-status.srt-running { background: #3b82f6; }
.srt-status.srt-done { background: #10b981; }
.srt-status.srt-error { background: #ef4444; }
.srt-jump { margin-left: auto; color: var(--accent-color, #3b82f6); text-decoration: none; }
.srt-jump:hover { text-decoration: underline; }
</style>
