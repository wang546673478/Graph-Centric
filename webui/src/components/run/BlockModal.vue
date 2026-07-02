<!--
  BlockModal — v2 spec §4.3.

  When the agent surfaces a `Block` (Clarifying saturated, Explore
  saturated, manual Block from the model), this modal pops up with
  three options:
    (a) Provide a more specific answer (opens the composer)
    (b) Force the agent to the next phase (emit ProposePatch)
    (c) Abort the run

  The Block state is detected from the loop_state WS event when
  `d.kind === 'Paused'` and the question starts with `[block]`.
-->
<template>
  <div v-if="open" class="block-modal-backdrop" @click.self="onCancel">
    <div class="block-modal">
      <div class="block-header">
        <span class="block-icon">⛔</span>
        <h3>{{ title }}</h3>
      </div>
      <div class="block-body">
        <p class="block-reason">{{ reason }}</p>
        <p v-if="hint" class="block-hint">💡 {{ hint }}</p>
        <div v-if="suggestion" class="block-suggestion">
          建议操作:<br>
          <code v-for="line in suggestion.split('\n')" :key="line">{{ line }}<br></code>
        </div>
      </div>
      <div class="block-actions">
        <button class="btn-secondary" @click="onCancel">取消</button>
        <button class="btn-secondary" @click="onAbort">中止 run</button>
        <button class="btn-primary" @click="onForce">强制下一阶段</button>
        <button class="btn-primary" @click="onAnswer">提供答复</button>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'

const props = defineProps<{
  open: boolean
  question: string
}>()

const emit = defineEmits<{
  (e: 'answer'): void
  (e: 'force'): void
  (e: 'abort'): void
  (e: 'cancel'): void
}>()

const reason = computed(() => {
  const q = props.question || ''
  if (q.includes('信息密度已饱和')) return 'agent 反复追问,信息密度已饱和'
  if (q.includes('重复追问同一话题')) return 'agent 在重复追问同一话题'
  if (q.includes('探索无收敛')) return 'agent 探索无收敛'
  if (q.includes('重复探索同一问题')) return 'agent 在重复探索同一问题'
  return q.replace(/^\[block\]\s*/, '') || 'agent 已暂停,等待用户裁决'
})

const hint = computed(() => {
  const q = props.question || ''
  if (q.includes('信息密度已饱和')) return '请直接给一个更明确的答复,或回复「继续」强制让 agent 进入下一阶段。'
  if (q.includes('重复追问')) return '请换个角度回答,或回复「继续」让 agent 跳过这一轮。'
  if (q.includes('探索无收敛')) return '模型对此问题可能找不到答案。请考虑:(a) 提供更多上下文;(b) 回复「继续」强制 commit;(c) 中止。'
  return ''
})

const title = computed(() => {
  const q = props.question || ''
  if (q.includes('澄清') || q.includes('追问')) return 'Clarifying Blocked'
  if (q.includes('探索') || q.includes('Explore')) return 'Explore Blocked'
  return 'Agent Blocked'
})

const suggestion = computed(() => {
  const q = props.question || ''
  if (q.includes('信息密度已饱和')) return '继续\n中止'
  if (q.includes('重复追问')) return '继续\n中止'
  if (q.includes('探索无收敛')) return '继续\n中止'
  return ''
})

function onAnswer() { emit('answer') }
function onForce() { emit('force') }
function onAbort() { emit('abort') }
function onCancel() { emit('cancel') }
</script>

<style scoped>
.block-modal-backdrop {
  position: fixed; inset: 0;
  background: rgba(0,0,0,0.45);
  display: flex; align-items: center; justify-content: center;
  z-index: 1000;
}
.block-modal {
  background: var(--bg-primary, #fff);
  border-radius: 8px;
  min-width: 420px; max-width: 600px;
  padding: 16px 20px;
  box-shadow: 0 4px 20px rgba(0,0,0,0.15);
}
.block-header { display: flex; align-items: center; gap: 10px; }
.block-icon { font-size: 22px; }
.block-header h3 { margin: 0; }
.block-body { margin: 12px 0; }
.block-reason { color: var(--text-primary); margin: 0 0 8px 0; }
.block-hint { color: var(--text-secondary); font-size: 13px; margin: 0 0 8px 0; }
.block-suggestion { background: var(--bg-secondary, #f5f5f5); padding: 8px; border-radius: 4px; font-size: 12px; }
.block-suggestion code { font-family: monospace; }
.block-actions { display: flex; gap: 8px; justify-content: flex-end; margin-top: 16px; }
.btn-primary, .btn-secondary {
  padding: 6px 14px; border-radius: 4px;
  border: 1px solid var(--border-color, #ccc);
  cursor: pointer; font-size: 13px;
}
.btn-primary { background: var(--accent-color, #3b82f6); color: white; border-color: var(--accent-color, #3b82f6); }
.btn-secondary { background: var(--bg-secondary, #f5f5f5); }
.btn-primary:hover, .btn-secondary:hover { opacity: 0.85; }
</style>
