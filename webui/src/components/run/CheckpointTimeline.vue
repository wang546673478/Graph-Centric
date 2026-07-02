<!--
  CheckpointTimeline — v2 spec §4.6.

  Scrubber along the bottom of the run view. Each checkpoint is
  a small circle on a horizontal line; click any one to load
  that snapshot. The current "live" run is on the far right.

  Uses the GET /api/runs/:id/checkpoints endpoint for the list
  and GET /api/runs/:id/checkpoints/:idx for the snapshot.
-->
<template>
  <div v-if="checkpoints.length > 0" class="checkpoint-timeline">
    <div class="ct-header">
      <span>📸 Checkpoints</span>
      <span class="ct-count">{{ checkpoints.length }} 个</span>
    </div>
    <div class="ct-bar">
      <div
        v-for="(cp, i) in checkpoints"
        :key="cp.index"
        class="ct-tick"
        :class="{ active: i === selected }"
        :style="{ left: position(cp) + '%' }"
        :title="`#${cp.index} · r${cp.round} · ${cp.node_count}n/${cp.edge_count}e · ${cp.phase}`"
        @click="onSelect(i)"
      />
      <div class="ct-line" />
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'

const props = defineProps<{
  checkpoints: { index: number; round: number; phase: string; node_count: number; edge_count: number }[]
  selected: number
}>()

const emit = defineEmits<{
  (e: 'select', index: number): void
}>()

function position(cp: any) {
  if (props.checkpoints.length <= 1) return 0
  const i = props.checkpoints.findIndex(c => c.index === cp.index)
  return (i / (props.checkpoints.length - 1)) * 100
}

function onSelect(i: number) {
  emit('select', props.checkpoints[i].index)
}
</script>

<style scoped>
.checkpoint-timeline {
  background: var(--bg-secondary, #f5f5f5);
  border-radius: 6px;
  padding: 6px 12px;
  margin: 6px 0;
  font-size: 12px;
}
.ct-header { display: flex; justify-content: space-between; color: var(--text-secondary); }
.ct-bar { position: relative; height: 20px; margin-top: 4px; }
.ct-line {
  position: absolute; top: 50%; left: 0; right: 0;
  height: 2px; background: #d1d5db; transform: translateY(-50%);
}
.ct-tick {
  position: absolute; top: 50%;
  width: 10px; height: 10px;
  border-radius: 50%;
  background: #6366f1;
  transform: translate(-50%, -50%);
  cursor: pointer;
  transition: transform 0.15s;
  z-index: 1;
}
.ct-tick:hover { transform: translate(-50%, -50%) scale(1.4); }
.ct-tick.active { background: #ef4444; }
</style>
