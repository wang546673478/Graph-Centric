<script setup lang="ts">
import { onMounted, provide, ref, watch } from 'vue'
import Sidebar from './components/layout/Sidebar.vue'
import TopBar from './components/shared/TopBar.vue'
import ShortcutsHelp from './components/shared/ShortcutsHelp.vue'
import { useShortcuts } from './composables/useShortcuts'
import { activeRunId, detailMode, runs, loadRuns } from './composables/useRunSocket'

provide('activeRunId', activeRunId)
provide('detailMode', detailMode)
provide('runs', runs)

const SIDEBAR_KEY = 'gc-sidebar-collapsed'
const RIGHT_KEY = 'gc-right-collapsed'
const sidebarCollapsed = ref(localStorage.getItem(SIDEBAR_KEY) === '1')
const rightCollapsed = ref(localStorage.getItem(RIGHT_KEY) === '1')
watch(sidebarCollapsed, (v) => localStorage.setItem(SIDEBAR_KEY, v ? '1' : '0'))
watch(rightCollapsed, (v) => localStorage.setItem(RIGHT_KEY, v ? '1' : '0'))

// Global keyboard shortcuts. "/" focuses search/composer; "?" toggles help.
const { showHelp, isMac } = useShortcuts()

onMounted(() => { loadRuns() })
</script>

<template>
  <div class="app-shell">
    <aside class="col-left" :class="{ collapsed: sidebarCollapsed }">
      <Sidebar />
    </aside>
    <button class="rail-toggle left" @click="sidebarCollapsed = !sidebarCollapsed"
      :title="sidebarCollapsed ? '展开导航' : '收起导航'">
      {{ sidebarCollapsed ? '›' : '‹' }}
    </button>

    <div class="col-center">
      <TopBar />
      <main class="main-content"><router-view /></main>
    </div>

    <button class="rail-toggle right" @click="rightCollapsed = !rightCollapsed"
      :title="rightCollapsed ? '展开侧栏' : '收起侧栏'">
      {{ rightCollapsed ? '‹' : '›' }}
    </button>
    <aside class="col-right" :class="{ collapsed: rightCollapsed }">
      <div class="right-placeholder">
        <p>对话 / 顾问面板</p>
        <span>P3 在此填充</span>
      </div>
    </aside>

    <ShortcutsHelp v-if="showHelp" :isMac="isMac" @close="showHelp = false" />
  </div>
</template>

<style scoped>
.app-shell { display: flex; height: 100vh; overflow: hidden; background: var(--bg); }
.col-left {
  width: 220px; flex-shrink: 0; overflow: hidden;
  transition: width 0.18s ease;
}
.col-left.collapsed { width: 0; }
.col-center { flex: 1; display: flex; flex-direction: column; min-width: 0; }
.main-content { flex: 1; overflow-y: auto; display: flex; flex-direction: column; }
.col-right {
  width: 340px; flex-shrink: 0; overflow: hidden;
  background: var(--bg-panel); border-left: 1px solid var(--border);
  transition: width 0.18s ease;
}
.col-right.collapsed { width: 0; border-left: none; }
.right-placeholder {
  padding: 20px 16px; color: var(--text-muted); font-size: 0.8rem;
  display: flex; flex-direction: column; gap: 4px;
}
.right-placeholder span { font-size: 0.7rem; opacity: 0.6; }
.rail-toggle {
  width: 14px; flex-shrink: 0; background: var(--bg-panel);
  border: none; border-right: 1px solid var(--border);
  color: var(--text-muted); cursor: pointer; font-size: 0.7rem; padding: 0;
}
.rail-toggle.right { border-right: none; border-left: 1px solid var(--border); }
.rail-toggle:hover { color: var(--accent); background: var(--bg-hover); }
</style>
