<script setup lang="ts">
import { ref, watch, nextTick } from 'vue'
import { useI18n } from '../../composables/useI18n'
const thinkingExpanded = ref(false)
const { t } = useI18n()

const props = defineProps<{ messages: { role: string; content: string }[]; status: string; error: string }>()
const el = ref<HTMLElement>()

watch(() => props.messages.length, async () => { await nextTick(); if (el.value) el.value.scrollTop = el.value.scrollHeight })

function roleClass(r: string) {
  if (r === 'user') return 'user'
  if (r === 'assistant' || r === 'agent' || r === 'assistant_streaming') return 'assistant'
  if (r === 'tool_result') return 'tool'
  if (r === 'error') return 'error'
  if (r === 'cascade' || r === 'model' || r === 'checkpoint') return 'detail'
  return ''
}
function showRoleTag(r: string): boolean {
  return r !== 'assistant_streaming'
}

function roleLabel(r: string): string {
  const zh: Record<string, string> = {
    user: '用户', assistant: '助手', agent: '代理',
    tool_result: '工具结果', error: '错误',
    ask_user: '询问用户', block: '阻塞', explore: '探索',
    cascade: '级联回溯', model: '模型调用', checkpoint: '检查点',
    thinking: '思考', assistant_streaming: '流式生成',
  }
  return zh[r] || r
}
</script>

<template>
  <div class="transcript" ref="el">
    <div v-if="!messages.length" class="empty">{{ t('transcript.empty') }}</div>
    <template v-for="(m, i) in messages" :key="i">
      <div v-if="m.role === 'thinking'" class="msg thinking-msg">
        <div class="thinking-toggle" @click="thinkingExpanded = !thinkingExpanded">
          {{ thinkingExpanded ? '🔽' : '💭' }} Thinking ({{ (m.content || '').length }} chars)
        </div>
        <pre v-if="thinkingExpanded" class="thinking-body">{{ m.content }}</pre>
      </div>
      <div v-else class="msg" :class="roleClass(m.role)">
        <span v-if="showRoleTag(m.role)" class="role-tag">{{ roleLabel(m.role) }}</span>
        <pre class="body">{{ m.content }}</pre>
      </div>
    </template>
    <div v-if="status === 'Running' || status === 'graph'" class="thinking">💭 {{ t('transcript.thinking') }}</div>
    <div v-if="error" class="msg error"><span class="role-tag">error</span><pre class="body">{{ error }}</pre></div>
  </div>
</template>

<style scoped>
.transcript { flex: 1; overflow-y: auto; padding: 12px; }
.empty { color: var(--text-muted); font-size: 0.85rem; padding: 24px; text-align: center; }
.msg { margin: 4px 0; border-radius: 6px; padding: 6px 10px; }
.msg.user { background: var(--bg-hover); }
.msg.assistant { background: transparent; }
.msg.tool { background: transparent; }
.msg.tool .body { font-family: var(--font-mono); font-size: 0.75rem; color: var(--text-muted); }
.msg.error { border: 1px solid var(--danger); }
.msg.error .body { color: var(--danger); }
.msg.detail { font-size: 0.75rem; }
.msg.detail .body { color: var(--warning); }
.role-tag { font-size: 0.65rem; text-transform: uppercase; color: var(--text-muted); margin-bottom: 2px; display: block; }
.body { white-space: pre-wrap; word-break: break-word; font-size: 0.85rem; margin: 0; font-family: var(--font); }
.thinking { padding: 8px; color: var(--text-muted); font-size: 0.8rem; font-style: italic; }
.thinking-msg { background: #fef3c7; border: 1px solid #fcd34d; border-radius: 6px; }
.thinking-toggle { cursor: pointer; padding: 4px 8px; font-size: 0.72rem; color: #92400e; user-select: none; }
.thinking-toggle:hover { background: #fde68a; border-radius: 6px; }
.thinking-body { font-size: 0.7rem; color: #78716c; white-space: pre-wrap; max-height: 300px; overflow-y: auto; padding: 4px 8px; margin: 0; }
</style>
