<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { api } from '../../composables/useRunSocket'
import { useI18n } from '../../composables/useI18n'
const { t } = useI18n()

const config = ref<any>({ model: {}, policy: {}, loop_tuning: {} })
const saved = ref(false)

onMounted(async () => { try { config.value = await api.get('/api/config') } catch { /* */ } })

async function save() {
  try { await api.post('/api/config', config.value); saved.value = true; setTimeout(() => saved.value = false, 2000) }
  catch (e) { alert(String(e)) }
}
</script>

<template>
  <div class="settings">
    <h2>{{ t('settings.title') }}</h2>
    <section>
      <h3>{{ t('settings.model') }}</h3>
      <label>{{ t('settings.baseUrl') }} <input v-model="config.model.base_url" /></label>
      <label>{{ t('settings.fastModel') }} <input v-model="config.model.fast_model" /></label>
      <label>{{ t('settings.deepModel') }} <input v-model="config.model.deep_model" /></label>
    </section>
    <section>
      <h3>{{ t('settings.policy') }}</h3>
      <label>{{ t('settings.maxConcurrent') }} <input type="number" v-model.number="config.policy.max_concurrent_subagents" /></label>
    </section>
    <section>
      <h3>{{ t('settings.loopTuning') }}</h3>
      <label>{{ t('settings.maxRounds') }} <input type="number" v-model.number="config.loop_tuning.max_rounds" /></label>
      <label><input type="checkbox" v-model="config.loop_tuning.cascade_backtrack" /> {{ t('settings.cascadeBacktrack') }}</label>
    </section>
    <button class="primary" @click="save">{{ saved ? t('settings.saved') : t('settings.save') }}</button>
  </div>
</template>

<style scoped>
.settings { padding: 24px; max-width: 600px; }
h2 { margin-bottom: 16px; }
h3 { color: var(--text-muted); font-size: 0.75rem; text-transform: uppercase; margin: 16px 0 8px; }
label { display: block; margin: 6px 0; font-size: 0.85rem; }
input { margin-top: 2px; }
input[type="checkbox"] { width: auto; margin-right: 6px; }
button { margin-top: 16px; }
</style>
