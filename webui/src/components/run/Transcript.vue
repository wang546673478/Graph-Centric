<script setup lang="ts">
import { ref, watch, nextTick } from 'vue'
import { useI18n } from '../../composables/useI18n'
const { t } = useI18n()

const props = defineProps<{ messages: { role: string; content: string }[]; status: string; error: string }>()
const el = ref<HTMLElement>()

watch(() => props.messages.length, async () => { await nextTick(); if (el.value) el.value.scrollTop = el.value.scrollHeight })

function roleClass(r: string) {
  if (r === 'user') return 'user'
  if (r === 'assistant' || r === 'agent') return 'assistant'
  if (r === 'tool_result') return 'tool'
  if (r === 'error') return 'error'
  if (r === 'cascade' || r === 'model' || r === 'checkpoint') return 'detail'
  return ''
}
</script>

<template>
  <div class="transcript" ref="el">
    <div v-if="!messages.length" class="empty">{{ t('transcript.empty') }}</div>
    <div v-for="(m, i) in messages" :key="i" class="msg" :class="roleClass(m.role)">
      <span class="role-tag">{{ m.role }}</span>
      <pre class="body">{{ m.content }}</pre>
    </div>
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
</style>
