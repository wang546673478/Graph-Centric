<script setup lang="ts">
import { computed } from 'vue'
import { activeRunId, runs, findRun } from '../../composables/useRunSocket'
import { useI18n } from '../../composables/useI18n'
const { t } = useI18n()

const status = computed(() => {
  if (!activeRunId.value) return 'idle'
  const r = findRun(activeRunId.value)
  return r?.status || 'idle'
})
</script>

<template>
  <span class="status-pill" :class="status">{{ status === 'idle' ? t('status.idle') : status }}</span>
</template>

<style scoped>
.status-pill { font-size: 0.7rem; padding: 2px 8px; border-radius: 10px; }
.status-pill.Running, .status-pill.graph { background: var(--accent); color: #fff; }
.status-pill.Paused, .status-pill.paused { background: var(--warning); color: #000; }
.status-pill.Done, .status-pill.done { background: var(--success); color: #000; }
.status-pill.Error, .status-pill.error, .status-pill.Cancelled { background: var(--danger); color: #fff; }
.status-pill.idle { background: var(--bg-hover); color: var(--text-muted); }
</style>
