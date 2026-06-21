<script setup lang="ts">
import { ref, watch, nextTick } from 'vue'
import { useI18n } from '../../composables/useI18n'
import { renderMarkdown } from '../../composables/useMarkdown'

const { t } = useI18n()
const props = defineProps<{ messages: { role: string; content: string }[]; status: string; error: string }>()
const el = ref<HTMLElement>()
// Per-message thinking expand state (by index), instead of one global toggle.
const expandedThinking = ref<Set<number>>(new Set())
function toggleThinking(i: number) {
  const s = new Set(expandedThinking.value)
  s.has(i) ? s.delete(i) : s.add(i)
  expandedThinking.value = s
}

watch(() => props.messages.length, async () => { await nextTick(); if (el.value) el.value.scrollTop = el.value.scrollHeight })

function isAdvisor(r: string): boolean {
  return r === 'advisor' || r.endsWith(':advisor')
}
function isThinking(r: string): boolean {
  return r === 'thinking' || r.startsWith('thinking:')
}
function roleClass(r: string) {
  if (isAdvisor(r)) return 'advisor'
  if (r === 'user') return 'user'
  if (r === 'assistant' || r === 'agent' || r.startsWith('stream:')) return 'assistant'
  if (r === 'tool_result') return 'tool'
  if (r === 'error') return 'error'
  if (r === 'cascade' || r === 'model' || r === 'checkpoint') return 'detail'
  return ''
}
function showRoleTag(r: string): boolean {
  return !r.startsWith('stream:') && !isThinking(r)
}
function useMd(r: string): boolean {
  // Render markdown for assistant/advisor/user prose; keep tool/detail/error monospace plain.
  return r === 'user' || r === 'assistant' || r === 'agent' || isAdvisor(r) || r.startsWith('stream:')
}
function looksLikeStepJson(c: string): boolean {
  const t = (c || '').trim()
  return t.startsWith('{') && t.includes('"step"')
}

function roleLabel(r: string): string {
  if (r.startsWith('stream:')) { const c = r.slice(7); return c === 'advisor' ? '顾问' : `流式:${c}` }
  if (r.startsWith('thinking:')) { const c = r.slice(9); return `思考:${c}` }
  const zh: Record<string, string> = {
    user: '用户', assistant: '助手', agent: '代理', advisor: '顾问',
    tool_result: '工具结果', error: '错误',
    ask_user: '询问用户', block: '阻塞', explore: '探索',
    cascade: '级联回溯', model: '模型调用', checkpoint: '检查点',
    thinking: '思考',
  }
  return zh[r] || r
}
</script>

<template>
  <div class="transcript" ref="el">
    <div v-if="!messages.length" class="empty">{{ t('transcript.empty') }}</div>
    <template v-for="(m, i) in messages" :key="i">
      <!-- Thinking block: per-message collapsible -->
      <div v-if="isThinking(m.role)" class="msg thinking-msg">
        <div class="thinking-toggle" @click="toggleThinking(i)">
          {{ expandedThinking.has(i) ? '🔽' : '💭' }} {{ roleLabel(m.role) }} ({{ (m.content || '').length }} chars)
        </div>
        <pre v-if="expandedThinking.has(i)" class="thinking-body">{{ m.content }}</pre>
      </div>
      <!-- Normal message — skip bare step JSON that slipped through -->
      <div v-else-if="!looksLikeStepJson(m.content)" class="msg" :class="roleClass(m.role)">
        <span v-if="showRoleTag(m.role)" class="role-tag">{{ roleLabel(m.role) }}</span>
        <div v-if="useMd(m.role)" class="md-body" v-html="renderMarkdown(m.content)"></div>
        <pre v-else class="body">{{ m.content }}</pre>
      </div>
    </template>
    <div v-if="status === 'Running' || status === 'graph'" class="thinking-indicator">💭 {{ t('transcript.thinking') }}</div>
    <div v-if="error" class="msg error"><span class="role-tag">error</span><pre class="body">{{ error }}</pre></div>
  </div>
</template>

<style scoped>
.transcript { flex: 1; overflow-y: auto; padding: 12px; }
.empty { color: var(--text-muted); font-size: 0.85rem; padding: 24px; text-align: center; }
.msg { margin: 8px 0; border-radius: 8px; padding: 8px 12px; }
.msg.user { background: var(--accent-soft); border: 1px solid var(--border); }
.msg.assistant { background: var(--bg-hover); }
.msg.advisor { background: var(--success-soft); border-left: 3px solid var(--success); }
.msg.tool { background: var(--bg-hover); border: 1px solid var(--border); }
.msg.tool .body { font-family: var(--font-mono); font-size: 0.75rem; color: var(--text-muted); }
.msg.error { border: 1px solid var(--danger); background: var(--danger-soft); }
.msg.error .body { color: var(--danger); }
.msg.detail { font-size: 0.75rem; }
.msg.detail .body { color: var(--warning); }
.role-tag { font-size: 0.65rem; text-transform: uppercase; color: var(--text-muted); margin-bottom: 4px; display: block; letter-spacing: 0.03em; }
.msg.advisor .role-tag { color: var(--success); }
.body { white-space: pre-wrap; word-break: break-word; font-size: 0.85rem; line-height: 1.6; margin: 0; font-family: var(--font); }
.thinking-indicator { padding: 8px; color: var(--text-muted); font-size: 0.8rem; font-style: italic; }
.thinking-msg { background: var(--warning-soft); border: 1px solid var(--warning); border-radius: 8px; margin: 8px 0; }
.thinking-toggle { cursor: pointer; padding: 6px 10px; font-size: 0.72rem; color: var(--warning); user-select: none; }
.thinking-toggle:hover { filter: brightness(1.1); }
.thinking-body { font-size: 0.72rem; color: var(--text-muted); white-space: pre-wrap; max-height: 300px; overflow-y: auto; padding: 4px 10px 8px; margin: 0; font-family: var(--font-mono); }
</style>
