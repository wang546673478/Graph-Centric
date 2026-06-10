<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { api } from '../../composables/useRunSocket'

const config = ref<any>({ model: {}, policy: {}, loop_tuning: {} })
const saved = ref(false)

onMounted(async () => { try { config.value = await api.get('/api/config') } catch { /* */ } })

async function save() {
  try {
    await api.post('/api/config', config.value)
    saved.value = true
    setTimeout(() => saved.value = false, 2000)
  } catch (e) { alert(String(e)) }
}
</script>

<template>
  <div class="settings">
    <h2>Engine Configuration</h2>

    <section>
      <h3>Model</h3>
      <label>Base URL <input v-model="config.model.base_url" /></label>
      <label>Fast Model <input v-model="config.model.fast_model" /></label>
      <label>Deep Model <input v-model="config.model.deep_model" /></label>
    </section>

    <section>
      <h3>Policy</h3>
      <label>Max Concurrent Subagents <input type="number" v-model.number="config.policy.max_concurrent_subagents" /></label>
    </section>

    <section>
      <h3>Loop Tuning</h3>
      <label>Max Rounds <input type="number" v-model.number="config.loop_tuning.max_rounds" /></label>
      <label><input type="checkbox" v-model="config.loop_tuning.cascade_backtrack" /> Cascade Backtracking</label>
    </section>

    <button class="primary" @click="save">{{ saved ? '✓ Saved' : 'Save Config' }}</button>
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
