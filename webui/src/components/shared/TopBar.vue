<script setup lang="ts">
import DetailModeToggle from './DetailModeToggle.vue'
import StatusPill from './StatusPill.vue'
import { useI18n, locale } from '../../composables/useI18n'
import { activeRunId } from '../../composables/useRunSocket'
import { useRouter } from 'vue-router'

const { t } = useI18n()
const router = useRouter()

function newChat() {
  activeRunId.value = null
  router.push('/')
}

function toggleLang() {
  locale.value = locale.value === 'en' ? 'zh-CN' : 'en'
}
</script>

<template>
  <header class="topbar">
    <nav>
      <router-link to="/">{{ t('nav.run') }}</router-link>
      <router-link to="/skills">{{ t('nav.skills') }}</router-link>
      <router-link to="/settings">{{ t('nav.settings') }}</router-link>
    </nav>
    <div class="right">
      <button class="primary new-chat-btn" @click="newChat">{{ t('run.newChat') }}</button>
      <button class="lang-btn" @click="toggleLang" :title="locale === 'en' ? '切换到中文' : 'Switch to English'">
        {{ locale === 'en' ? '中文' : 'EN' }}
      </button>
      <DetailModeToggle />
      <StatusPill />
    </div>
  </header>
</template>

<style scoped>
.topbar {
  display: flex; align-items: center; justify-content: space-between;
  padding: 0 16px; background: var(--bg-panel); border-bottom: 1px solid var(--border);
  min-height: 40px;
}
nav { display: flex; gap: 2px; }
nav a {
  padding: 10px 12px; color: var(--text-muted); text-decoration: none;
  border-radius: 0; font-size: 0.78rem; border-bottom: 2px solid transparent;
}
nav a:hover, nav a.router-link-active {
  color: var(--text); border-bottom-color: var(--accent);
}
.right { display: flex; gap: 10px; align-items: center; }
.lang-btn {
  background: var(--bg-hover); color: var(--text-muted); border: 1px solid var(--border);
  padding: 2px 8px; border-radius: 4px; font-size: 0.7rem; cursor: pointer;
}
.lang-btn:hover { color: var(--text); border-color: var(--accent); }
</style>
