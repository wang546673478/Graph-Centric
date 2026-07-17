<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { useI18n } from '../../composables/useI18n'

const { t } = useI18n()
const props = defineProps<{ disabled: boolean }>()
const emit = defineEmits<{ send: [task: string] }>()

const KEY = 'gc-composer-history'
const TEMPLATES = [
  '为这段代码加单元测试,覆盖边界情况',
  '重构这个函数,提升可读性,但保持行为不变',
  '解释这段代码做了什么,关键点用 // 注释',
]

const msg = ref('')
const showHistory = ref(false)
const history = ref<string[]>(loadHistory())

function loadHistory(): string[] {
  try { return JSON.parse(localStorage.getItem(KEY) || '[]') } catch { return [] }
}
function pushHistory(t: string) {
  const h = [t, ...history.value.filter(x => x !== t)].slice(0, 8)
  history.value = h
  try { localStorage.setItem(KEY, JSON.stringify(h)) } catch { /* quota */ }
}

function send() {
  const v = msg.value.trim()
  if (!v || props.disabled) return
  msg.value = ''
  showHistory.value = false
  pushHistory(v)
  emit('send', v)
}

function pickTemplate(t: string) {
  msg.value = t
  msgEl.value?.focus()
}

function pickHistory(t: string) {
  msg.value = t
  showHistory.value = false
}

const msgEl = ref<HTMLTextAreaElement | null>(null)
function onKeydown(e: KeyboardEvent) {
  // Enter to send, Shift+Enter for newline.
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
    e.preventDefault()
    send()
  }
}
onMounted(() => { msgEl.value?.focus() })
</script>

<template>
  <div class="composer">
    <div class="composer-row">
      <button v-if="history.length" class="hist-btn" @click="showHistory = !showHistory" :title="`历史 (${history.length})`">▾</button>
      <textarea
        ref="msgEl"
        v-model="msg"
        :disabled="disabled"
        :placeholder="t('composer.placeholder')"
        rows="2"
        class="composer-input"
        @keydown="onKeydown"
      />
      <button class="primary send-btn" :disabled="disabled" @click="send">{{ disabled ? '…' : t('composer.send') }}</button>
    </div>
    <div v-if="showHistory" class="history">
      <div class="history-header">最近发送</div>
      <div v-for="(h, i) in history" :key="i" class="history-item" @click="pickHistory(h)">
        {{ h.length > 60 ? h.slice(0, 60) + '…' : h }}
      </div>
      <div class="history-clear" @click="history = []; try { localStorage.removeItem(KEY) } catch { /* blocked */ }">清空</div>
    </div>
    <div v-if="!msg.length && !showHistory" class="templates">
      <span class="tpl-label">模板:</span>
      <button v-for="(t, i) in TEMPLATES" :key="i" class="tpl" @click="pickTemplate(t)">{{ t.slice(0, 20) }}…</button>
    </div>
  </div>
</template>

<style scoped>
.composer { display: flex; flex-direction: column; gap: 4px; padding: 10px 12px; border-top: 1px solid var(--border); background: var(--bg-panel); }
.composer-row { display: flex; gap: 6px; align-items: stretch; }
.composer-input { flex: 1; resize: vertical; min-height: 38px; max-height: 160px; font-family: var(--font); font-size: 0.85rem; padding: 6px 10px; border: 1px solid var(--border); border-radius: 4px; background: var(--bg); color: var(--text); }
.composer-input:focus { outline: none; border-color: var(--accent); }
.send-btn { padding: 6px 16px; align-self: stretch; }
.hist-btn { width: 32px; padding: 0; font-size: 0.9rem; color: var(--text-muted); background: var(--bg); border: 1px solid var(--border); border-radius: 4px; cursor: pointer; }
.hist-btn:hover { color: var(--accent); border-color: var(--accent); }
.history { background: var(--bg); border: 1px solid var(--border); border-radius: 4px; padding: 6px; max-height: 180px; overflow-y: auto; }
.history-header { font-size: 0.65rem; text-transform: uppercase; color: var(--text-muted); padding: 0 0 4px; }
.history-item { font-size: 0.78rem; padding: 4px 6px; cursor: pointer; border-radius: 3px; }
.history-item:hover { background: var(--accent-soft); }
.history-clear { font-size: 0.7rem; color: var(--text-muted); padding: 4px 6px; cursor: pointer; text-align: right; }
.templates { display: flex; gap: 4px; align-items: center; flex-wrap: wrap; padding: 2px 0 0; }
.tpl-label { font-size: 0.7rem; color: var(--text-muted); }
.tpl { font-size: 0.72rem; padding: 2px 8px; background: var(--bg); border: 1px solid var(--border); border-radius: 12px; color: var(--text-muted); cursor: pointer; }
.tpl:hover { color: var(--accent); border-color: var(--accent); }
</style>
