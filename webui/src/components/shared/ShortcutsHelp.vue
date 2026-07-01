<script setup lang="ts">
import { ref } from 'vue'
defineProps<{ isMac?: boolean }>()
const emit = defineEmits<{ close: [] }>()
</script>

<template>
  <div class="help-mask" @click.self="emit('close')">
    <div class="help-modal">
      <div class="help-header">
        <span class="title">⌨ 键盘快捷键</span>
        <button class="close" @click="emit('close')">×</button>
      </div>
      <div class="help-body">
        <div class="row">
          <kbd>/</kbd>
          <span>聚焦搜索/输入框</span>
        </div>
        <div class="row">
          <kbd>?</kbd>
          <span>显示/隐藏这个帮助</span>
        </div>
        <div class="row">
          <kbd>Esc</kbd>
          <span>关闭弹窗 / 取消</span>
        </div>
        <div class="row">
          <kbd>{{ isMac ? '⌘' : 'Ctrl' }} + S</kbd>
          <span>立即保存（可后续扩展）</span>
        </div>
        <div class="row">
          <kbd>Esc</kbd>
          <span>从输入框退出聚焦</span>
        </div>
      </div>
      <div class="help-footer">
        <span class="hint">提示：sidebar 的 "+ 新建" 按钮也会清空当前 run</span>
      </div>
    </div>
  </div>
</template>

<style scoped>
.help-mask { position: fixed; inset: 0; background: rgba(0,0,0,0.4); z-index: 100; display: flex; align-items: center; justify-content: center; }
.help-modal { background: var(--bg-panel); border-radius: 8px; box-shadow: 0 10px 30px rgba(0,0,0,0.3); min-width: 360px; max-width: 90vw; }
.help-header { display: flex; align-items: center; justify-content: space-between; padding: 12px 16px; border-bottom: 1px solid var(--border); }
.help-header .title { font-weight: 600; font-size: 0.95rem; }
.help-header .close { background: none; border: none; font-size: 1.3rem; color: var(--text-muted); cursor: pointer; padding: 0 4px; }
.help-body { padding: 12px 16px; }
.help-body .row { display: flex; align-items: center; gap: 12px; padding: 4px 0; font-size: 0.82rem; }
.help-body kbd { background: var(--bg); border: 1px solid var(--border); border-bottom-width: 2px; padding: 2px 8px; border-radius: 4px; font-family: var(--font-mono); font-size: 0.78rem; min-width: 80px; text-align: center; }
.help-footer { padding: 8px 16px; border-top: 1px solid var(--border); font-size: 0.72rem; color: var(--text-muted); }
</style>
