<!--
  ExplorerBar — v2 spec §4.4.

  Visible whenever the agent is in Filling/Expanding with active
  exploration. Shows:
  - a progress bar for the explore iter counter
  - the recent question/answer log (last N)
  - tier hint badges when the iter crosses 100/150

  Hidden when explorer_iter is 0 (no exploration in progress).
-->
<template>
  <div v-if="visible" class="explorer-bar">
    <div class="explorer-header">
      <span class="explorer-title">🔍 探索中 <b>{{ iter }}</b> / 200</span>
      <span :class="['explorer-tier', tierClass]" v-if="tier !== 'none'">{{ tierLabel }}</span>
    </div>
    <div class="explorer-progress">
      <div class="explorer-fill" :style="{ width: pct + '%', background: tierColor }" />
    </div>
    <div v-if="recent.length" class="explorer-log">
      <div v-for="(q, i) in recent" :key="i" class="explorer-item">
        <span class="explorer-q">#{{ i + 1 }}:</span> {{ q }}
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'

const props = defineProps<{
  iter: number
  recent: string[]
}>()

const visible = computed(() => props.iter > 0)
const pct = computed(() => Math.min(100, (props.iter / 200) * 100))
const tier = computed(() => {
  if (props.iter >= 150) return 'hard'
  if (props.iter >= 100) return 'soft'
  return 'none'
})
const tierLabel = computed(() => {
  if (tier.value === 'hard') return '🚨 150+ 警告'
  if (tier.value === 'soft') return '⚠️ 100+ 软提示'
  return ''
})
const tierClass = computed(() => {
  if (tier.value === 'hard') return 'hard'
  if (tier.value === 'soft') return 'soft'
  return ''
})
const tierColor = computed(() => {
  if (tier.value === 'hard') return '#ef4444'
  if (tier.value === 'soft') return '#f59e0b'
  return '#10b981'
})
</script>

<style scoped>
.explorer-bar {
  background: var(--bg-secondary, #f5f5f5);
  border-radius: 6px;
  padding: 8px 12px;
  margin: 6px 0;
  font-size: 12px;
}
.explorer-header { display: flex; align-items: center; gap: 8px; }
.explorer-title { color: var(--text-primary); }
.explorer-tier { padding: 1px 8px; border-radius: 4px; color: white; }
.explorer-tier.soft { background: #f59e0b; }
.explorer-tier.hard { background: #ef4444; }
.explorer-progress { height: 6px; background: #e5e7eb; border-radius: 3px; margin: 6px 0; overflow: hidden; }
.explorer-fill { height: 100%; transition: width 0.3s, background 0.3s; }
.explorer-log { margin-top: 4px; max-height: 100px; overflow-y: auto; }
.explorer-item { color: var(--text-secondary); padding: 2px 0; }
.explorer-q { color: var(--text-tertiary, #999); font-family: monospace; }
</style>
