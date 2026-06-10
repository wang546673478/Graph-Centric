<script setup lang="ts">
import Sidebar from './components/layout/Sidebar.vue'
import TopBar from './components/shared/TopBar.vue'
import { activeRunId, detailMode, runs, loadRuns } from './composables/useRunSocket'
import { onMounted, provide } from 'vue'

provide('activeRunId', activeRunId)
provide('detailMode', detailMode)
provide('runs', runs)

onMounted(() => { loadRuns() })
</script>

<template>
  <div class="app-shell">
    <Sidebar />
    <div class="main-area">
      <TopBar />
      <main class="main-content"><router-view /></main>
    </div>
  </div>
</template>

<style scoped>
.app-shell { display: flex; height: 100vh; overflow: hidden; }
.main-area { flex: 1; display: flex; flex-direction: column; min-width: 0; }
.main-content { flex: 1; overflow: hidden; display: flex; flex-direction: column; }
</style>
