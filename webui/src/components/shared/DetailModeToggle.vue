<script setup lang="ts">
import { ref } from 'vue'
import { useI18n } from '../../composables/useI18n'
const { t } = useI18n()
const on = ref(false)
defineEmits(['toggle'])
</script>
<template>
  <label class="toggle"><input type="checkbox" v-model="on" @change="$emit('toggle', on)" /> <span>{{ t('detail.label') }}</span></label>
</template>
<style scoped>.toggle{display:flex;align-items:center;gap:4px;font-size:.75rem;color:var(--text-muted);cursor:pointer}</style>
