<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { api } from '../../composables/useRunSocket'
import { useI18n } from '../../composables/useI18n'
const { t } = useI18n()

const config = ref<any>({ model: {}, policy: {}, loop_tuning: {} })
const saved = ref(false)
const showKey = ref(false)
const modelList = ref<string[]>([])
const fetching = ref(false)
const keyDirty = ref(false)
const origKey = ref('')
const profileName = ref('')
const profiles = ref<Record<string, any>>({})

onMounted(async () => {
  try {
    config.value = await api.get('/api/config')
    origKey.value = config.value.model?.api_key_masked || ''
    profiles.value = config.value.profiles || {}
  } catch { /* */ }
})

function onKeyInput() { keyDirty.value = true }

function switchProfile(name: string) {
  if (profiles.value[name]) {
    config.value.model = { ...profiles.value[name] }
    config.value.active_profile = name
    origKey.value = config.value.model?.api_key_masked || ''
    keyDirty.value = false
  }
}

function saveProfile() {
  const name = profileName.value.trim() || config.value.active_profile || 'default'
  profiles.value[name] = { ...config.value.model }
  config.value.profiles = profiles.value
  config.value.active_profile = name
  profileName.value = ''
  save()
}

function deleteProfile(name: string) {
  delete profiles.value[name]
  config.value.profiles = profiles.value
  if (config.value.active_profile === name) {
    config.value.active_profile = ''
  }
  save()
}

async function fetchModels() {
  const baseUrl = config.value.model?.base_url?.trim()
  if (!baseUrl) return
  fetching.value = true
  try {
    const key = config.value.model?.api_key || config.value.model?.api_key_masked || ''
    const resp = await fetch(`/api/models?base_url=${encodeURIComponent(baseUrl)}&api_key=${encodeURIComponent(key)}`)
    const data = await resp.json()
    modelList.value = data.models || []
  } catch (e) { alert(String(e)) }
  finally { fetching.value = false }
}

async function save() {
  if (!keyDirty.value) {
    // Don't overwrite the real key with the masked display value.
    config.value.model.api_key_masked = origKey.value
  }
  try {
    config.value = await api.post('/api/config', config.value)
    origKey.value = config.value.model?.api_key_masked || ''
    keyDirty.value = false
    saved.value = true; setTimeout(() => saved.value = false, 2000)
  } catch (e) { alert(String(e)) }
}
</script>

<template>
  <div class="settings">
    <h2>{{ t('settings.title') }}</h2>

    <!-- Profile bar -->
    <section>
      <h3>Profiles</h3>
      <div class="profile-bar">
        <select v-model="config.active_profile" @change="switchProfile(config.active_profile)" class="profile-select">
          <option value="">-- direct config --</option>
          <option v-for="(v, k) in profiles" :key="k" :value="k">{{ k }}</option>
        </select>
        <input v-model="profileName" placeholder="profile name" class="profile-input" />
        <button class="secondary" @click="saveProfile" :disabled="!profileName.trim() && !config.active_profile">💾 Save</button>
        <button v-if="config.active_profile" class="danger" @click="deleteProfile(config.active_profile)">🗑 Delete</button>
      </div>
      <div v-if="config.active_profile" class="hint">Profile: <b>{{ config.active_profile }}</b></div>
    </section>

    <section>
      <h3>{{ t('settings.model') }}</h3>
      <label>{{ t('settings.baseUrl') }} <input v-model="config.model.base_url" placeholder="https://api.deepseek.com/v1" /></label>
      <div class="fetch-row">
        <button class="secondary" @click="fetchModels" :disabled="fetching">
          {{ fetching ? 'Fetching…' : '🔍 Fetch Models' }}
        </button>
        <span v-if="modelList.length" class="hint">{{ modelList.length }} model(s) found</span>
      </div>
      <label>API Key
        <div class="key-row">
          <input :type="showKey ? 'text' : 'password'" v-model="config.model.api_key_masked" placeholder="sk-…" @input="onKeyInput" />
          <button class="secondary" @click="showKey = !showKey">{{ showKey ? '隐藏' : '显示' }}</button>
        </div>
      </label>
      <label>{{ t('settings.fastModel') }}
        <select v-if="modelList.length" v-model="config.model.fast_model">
          <option v-for="m in modelList" :key="m" :value="m">{{ m }}</option>
        </select>
        <input v-else v-model="config.model.fast_model" placeholder="deepseek-v4-flash" />
      </label>
      <label>{{ t('settings.deepModel') }}
        <select v-if="modelList.length" v-model="config.model.deep_model">
          <option v-for="m in modelList" :key="m" :value="m">{{ m }}</option>
        </select>
        <input v-else v-model="config.model.deep_model" placeholder="deepseek-v4-pro" />
      </label>
    </section>

    <section>
      <h3>{{ t('settings.policy') }}</h3>
      <label>{{ t('settings.maxConcurrent') }} <input type="number" v-model.number="config.policy.max_concurrent_subagents" /></label>
    </section>

    <section>
      <h3>{{ t('settings.loopTuning') }}</h3>
      <label>{{ t('settings.maxRounds') }} <input type="number" v-model.number="config.loop_tuning.max_rounds" /></label>
      <label><input type="checkbox" v-model="config.loop_tuning.cascade_backtrack" /> {{ t('settings.cascadeBacktrack') }}</label>
      <label><input type="checkbox" v-model="config.loop_tuning.thinking_enabled" /> Thinking Mode (DeepSeek)</label>
      <label v-if="config.loop_tuning.thinking_enabled">Reasoning Effort
        <select v-model="config.loop_tuning.reasoning_effort">
          <option value="high">high</option>
          <option value="max">max</option>
        </select>
      </label>
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
label input, label select { margin-top: 4px; }
select { width: 100%; padding: 8px 12px; border: 1px solid var(--border); border-radius: var(--radius); background: var(--bg); color: var(--text); font-size: 0.85rem; font-family: var(--font); }
input[type="checkbox"] { width: auto; margin-right: 6px; }
.key-row { display: flex; gap: 6px; margin-top: 4px; }
.key-row input { flex: 1; }
.key-row button { white-space: nowrap; font-size: 0.75rem; padding: 6px 10px; }
.fetch-row { display: flex; align-items: center; gap: 10px; margin: 8px 0; }
.fetch-row button { font-size: 0.75rem; padding: 5px 12px; }
.hint { font-size: 0.7rem; color: var(--accent); }
button.primary { margin-top: 8px; padding: 10px 24px; }
.profile-bar { display: flex; gap: 6px; align-items: center; flex-wrap: wrap; }
.profile-select { flex: 1; min-width: 120px; padding: 6px 8px; border: 1px solid var(--border); border-radius: var(--radius); background: var(--bg); color: var(--text); font-size: 0.8rem; font-family: var(--font); }
.profile-input { width: 120px; flex: none; }
.profile-bar button { font-size: 0.72rem; padding: 5px 8px; white-space: nowrap; }
</style>
