<script setup lang="ts">
import { ref } from 'vue'
import { useI18n } from '../../composables/useI18n'
const { t } = useI18n()
const props = defineProps<{ disabled: boolean }>()
const emit = defineEmits<{ send: [task: string] }>()
const msg = ref('')
function send() { const v = msg.value.trim(); if (!v || props.disabled) return; msg.value = ''; emit('send', v) }
</script>

<template>
  <div class="composer">
    <input v-model="msg" :disabled="disabled" :placeholder="t('composer.placeholder')" @keydown.enter="send" />
    <button class="primary" :disabled="disabled" @click="send">{{ disabled ? '…' : t('composer.send') }}</button>
  </div>
</template>

<style scoped>
.composer { display: flex; gap: 8px; padding: 12px; border-top: 1px solid var(--border); }
.composer input { flex: 1; }
.composer button { padding: 8px 20px; white-space: nowrap; }
</style>
