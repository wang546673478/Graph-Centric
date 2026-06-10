<script setup lang="ts">
import { ref } from 'vue'

const props = defineProps<{ disabled: boolean }>()
const emit = defineEmits<{ send: [task: string] }>()

const msg = ref('')

function send() {
  const t = msg.value.trim()
  if (!t || props.disabled) return
  msg.value = ''
  emit('send', t)
}
</script>

<template>
  <div class="composer">
    <input
      v-model="msg"
      :disabled="disabled"
      placeholder="Type a task…"
      @keydown.enter="send"
    />
    <button class="primary" :disabled="disabled" @click="send">
      {{ disabled ? '…' : 'Send' }}
    </button>
  </div>
</template>

<style scoped>
.composer { display: flex; gap: 8px; padding: 12px; border-top: 1px solid var(--border); }
.composer input { flex: 1; }
.composer button { padding: 8px 20px; white-space: nowrap; }
</style>
