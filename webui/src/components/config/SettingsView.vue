<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { api } from '../../composables/useRunSocket'
import { useI18n } from '../../composables/useI18n'
const { t } = useI18n()

const config = ref<any>({ model: {}, policy: {}, loop_tuning: {} })
const saved = ref(false)
const showKey = ref(false)

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
      <label>{{ t('settings.baseUrl') }} <input v-model="config.model.base_url" placeholder="https://api.deepseek.com/v1" /></label>
      <label>API Key
        <div class="key-row">
          <input :type="showKey ? 'text' : 'password'" v-model="config.model.api_key_masked" placeholder="sk-…" />
          <button class="secondary" @click="showKey = !showKey">{{ showKey ? '隐藏' : '显示' }}</button>
        </div>
      </label>
      <label>{{ t('settings.fastModel') }} <input v-model="config.model.fast_model" placeholder="deepseek-v4-flash" /></label>
      <label>{{ t('settings.deepModel') }} <input v-model="config.model.deep_model" placeholder="deepseek-v4-pro" /></label>
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
.settings { padding: 32px; max-width: 640px; }
h2 { font-size: 1.3rem; font-weight: 700; margin-bottom: 24px; letter-spacing: -0.01em; }
section { background: var(--bg-panel); border: 1px solid var(--border); border-radius: var(--radius); padding: 20px; margin-bottom: 16px; box-shadow: var(--shadow); }
h3 { color: var(--text-muted); font-size: 0.7rem; text-transform: uppercase; letter-spacing: 0.08em; margin-bottom: 12px; font-weight: 600; }
label { display: block; margin: 10px 0; font-size: 0.82rem; color: var(--text); }
label input { margin-top: 4px; }
input[type="checkbox"] { width: auto; margin-right: 6px; }
.key-row { display: flex; gap: 6px; margin-top: 4px; }
.key-row input { flex: 1; }
.key-row button { white-space: nowrap; font-size: 0.75rem; padding: 6px 10px; }
button.primary { margin-top: 8px; padding: 10px 24px; }
</style>
