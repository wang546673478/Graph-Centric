<script setup lang="ts">
import { ref, onMounted, watch } from 'vue'
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
const DEFAULTS = {
  max_rounds: 10,
  prompt: `对 Graph-Centric Agent 进行10轮自我优化。每轮选一个具体优化点，改完通过编译后自动重启进入下一轮。
优化范围包括后端Rust代码(src/)和前端Vue3界面(webui/src/)。

## 优化方向
### 后端 (src/)
- 降低unwrap/unsafe密度，提升代码健壮性
- 优化模块边界，减少大文件(>500行)
- 改善错误处理，用结构化错误替代字符串
- 参考 openclaw/opencode/CodeWhale 中的模式但不照搬

### 前端 (webui/src/)
- 参考 GitHub 上优秀AI agent项目的Web界面设计
- 优化现有Vue3组件的排版、配色、交互体验
- 改进对话区域的视觉层次感和可读性
- 增强3D关系图面板的可用性(标签、动画、布局)
- 设置页面和信息页面的信息架构优化

## 搜索外部项目 (Explore + WebSearch)
- 用 Explore 派子代理去 GitHub 搜索关键词
- 子代理有 web_search 工具可直接搜索

## 每轮工作流 (A→D)
1. 创建 A(当前问题)和 D(优化目标)
2. Explore 扫描 src/ 或 webui/src/ 找出具体问题点
3. 如果本轮需要外部参考: Explore + web_search 搜索 GitHub
4. ProposePatch: 仅修改1-3个相关文件
5. SubAgent执行修改(自动git commit+cargo check验证)
6. Review通过→本轮完成→自动编译重启进入下一轮

## 约束
- 每轮只改1-3个文件，必须编译通过
- 禁止引入新unwrap/unsafe，禁止删除测试
- 不改graph/mod.rs和graph/l1.rs(核心图结构)
- 外部项目只参考设计模式，不照搬代码
- 前端改动不引入新依赖(保持轻量)
- 第10轮结束自动停止`
}

const heartbeat = ref<any>(null)
const hbPrompt = ref(DEFAULTS.prompt)
const hbRounds = ref(DEFAULTS.max_rounds)

onMounted(async () => {
  try {
    config.value = await api.get('/api/config')
    origKey.value = config.value.model?.api_key_masked || ''
    profiles.value = config.value.profiles || {}
    heartbeat.value = await api.get('/api/heartbeat')
    if (heartbeat.value?.active && heartbeat.value?.prompt) {
      hbPrompt.value = heartbeat.value.prompt
    } else if (!heartbeat.value?.active) {
      // Load saved prompt from localStorage (edits survive refresh)
      const saved = localStorage.getItem('hb-prompt')
      if (saved) hbPrompt.value = saved
      const savedRounds = localStorage.getItem('hb-rounds')
      if (savedRounds) hbRounds.value = parseInt(savedRounds) || 10
    }
  } catch { /* */ }
})

// Persist prompt edits to localStorage on every keystroke.
watch(hbPrompt, (v) => localStorage.setItem('hb-prompt', v))
watch(hbRounds, (v) => localStorage.setItem('hb-rounds', String(v)))

async function startHeartbeat() {
  try {
    const body = { prompt: hbPrompt.value, max_rounds: hbRounds.value }
    heartbeat.value = await api.post('/api/heartbeat', body)
    heartbeat.value = { active: true, max_rounds: hbRounds.value, completed_rounds: 0, prompt: hbPrompt.value }
  } catch(e) { alert(String(e)) }
}
async function cancelHeartbeat() {
  try { heartbeat.value = await api.post('/api/heartbeat/cancel') } catch { /* */ }
  heartbeat.value = { active: false }
  hbPrompt.value = DEFAULTS.prompt
}
async function refreshHeartbeat() {
  try { heartbeat.value = await api.get('/api/heartbeat') } catch { /* */ }
}

function onKeyInput() { keyDirty.value = true }

async function switchProfile(name: string) {
  if (profiles.value[name]) {
    config.value.model = { ...profiles.value[name] }
    config.value.active_profile = name
    origKey.value = config.value.model?.api_key_masked || ''
    keyDirty.value = false
    // Auto-save on profile switch so env vars are set immediately.
    await save()
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
    // Real key in api_key (unmasked), user-typed in api_key_masked.
    const key = (config.value.model?.api_key && !config.value.model.api_key.includes('***'))
      ? config.value.model.api_key
      : (config.value.model?.api_key_masked || '')
    const resp = await fetch(`/api/models?base_url=${encodeURIComponent(baseUrl)}&api_key=${encodeURIComponent(key)}`)
    const data = await resp.json()
    modelList.value = data.models || []
  } catch (e) { alert(String(e)) }
  finally { fetching.value = false }
}

async function save() {
  if (!keyDirty.value) {
    config.value.model.api_key_masked = origKey.value
  }
  // Sync current model to the active profile.
  const ap = config.value.active_profile
  if (ap && profiles.value[ap]) {
    profiles.value[ap] = { ...config.value.model }
    config.value.profiles = { ...profiles.value }
  }
  try {
    const resp = await api.post('/api/config', config.value)
    // Update in-place to avoid reactivity issues with the dropdown.
    Object.assign(config.value, resp)
    origKey.value = resp.model?.api_key_masked || ''
    profiles.value = resp.profiles || {}
    keyDirty.value = false
    saved.value = true; setTimeout(() => saved.value = false, 2000)
  } catch (e) { alert(String(e)) }
}
</script>

<template>
  <div class="settings">
    <h2>{{ t('settings.title') }}</h2>

    <!-- Heartbeat -->
    <section class="heartbeat-section">
      <h3>🫀 自优化循环 (HeartBeat)</h3>
      <div v-if="heartbeat && heartbeat.active" class="hb-active">
        <div><b>状态:</b> 🔄 运行中 · 第 {{ heartbeat.completed_rounds || 0 }}/{{ heartbeat.max_rounds }} 轮</div>
        <div class="hb-prompt-ro">{{ heartbeat.prompt?.slice(0, 400) }}{{ (heartbeat.prompt?.length || 0) > 400 ? '…' : '' }}</div>
        <div v-if="heartbeat.current_run_id" class="hb-run-link">
          📋 <router-link :to="'/'">查看当前任务</router-link> ({{ heartbeat.current_run_id?.slice(0,8) }}…)
        </div>
        <div class="hb-actions">
          <button class="secondary" @click="refreshHeartbeat">🔄 刷新</button>
          <button class="danger" @click="cancelHeartbeat">⏹ 停止</button>
        </div>
      </div>
      <div v-else class="hb-idle">
        <div><b>状态:</b> ⏸ 未启动</div>
        <label>轮数 <input type="number" v-model.number="hbRounds" min="1" max="50" class="rounds-input" /></label>
        <textarea v-model="hbPrompt" rows="12" class="hb-textarea"></textarea>
        <p class="hint">提示词可自由修改。启动后不可改。</p>
        <button class="primary" @click="startHeartbeat">▶ 启动 {{ hbRounds }} 轮自优化</button>
      </div>
    </section>

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
.heartbeat-section { border-color: #a78bda; }
.hb-active { display: flex; flex-direction: column; gap: 8px; }
.hb-prompt { font-size: 0.7rem; color: var(--text-muted); max-height: 60px; overflow: hidden; cursor: pointer; }
.hb-textarea { width: 100%; font-size: 0.72rem; font-family: var(--font-mono); margin: 4px 0; }
.hb-prompt-ro { font-size: 0.72rem; color: var(--text-muted); max-height: 80px; overflow-y: auto; background: var(--bg); padding: 6px 8px; border-radius: 4px; white-space: pre-wrap; }
.hb-actions { display: flex; gap: 6px; margin-top: 4px; }
.hb-idle { display: flex; flex-direction: column; gap: 8px; }
.hb-idle .hint { font-size: 0.72rem; color: var(--text-muted); line-height: 1.5; }
.rounds-input { width: 60px; padding: 4px 6px; }
</style>
