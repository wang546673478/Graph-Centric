<script setup lang="ts">
import DetailModeToggle from './DetailModeToggle.vue'
import StatusPill from './StatusPill.vue'
import { useI18n, locale } from '../../composables/useI18n'
import { useTheme } from '../../composables/useTheme'

const { t } = useI18n()
const { theme, toggleTheme } = useTheme()

function toggleLang() {
  locale.value = locale.value === 'en' ? 'zh-CN' : 'en'
}
</script>

<template>
  <header class="topbar">
    <nav>
      <router-link to="/">{{ t('nav.run') }}</router-link>
      <router-link to="/runs">{{ t('nav.history') }}</router-link>
      <router-link to="/usage">Usage</router-link>
      <router-link to="/skills">{{ t('nav.skills') }}</router-link>
      <router-link to="/settings">{{ t('nav.settings') }}</router-link>
    </nav>
    <div class="right">
      <button class="shortcut-hint" @click="$root?.$el?.dispatchEvent?.(new KeyboardEvent('keydown', { key: '?' }))" title="按 ? 打开快捷键帮助">⌨ ?</button>
      <button class="lang-btn" @click="toggleLang" :title="locale === 'en' ? '切换到中文' : 'Switch to English'">
        {{ locale === 'en' ? '中文' : 'EN' }}
      </button>
      <button class="theme-btn" @click="toggleTheme"
        :title="theme === 'dark' ? '切换到浅色' : '切换到深色'">
        {{ theme === 'dark' ? '☀️' : '🌙' }}
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
.theme-btn {
  background: var(--bg-hover); border: 1px solid var(--border);
  padding: 2px 8px; border-radius: 4px; font-size: 0.85rem; cursor: pointer;
  line-height: 1;
}
.theme-btn:hover { border-color: var(--accent); }
.shortcut-hint { background: var(--bg-hover); border: 1px solid var(--border); padding: 2px 8px; border-radius: 4px; font-size: 0.72rem; cursor: pointer; color: var(--text-muted); }
.shortcut-hint:hover { color: var(--accent); border-color: var(--accent); }
</style>
