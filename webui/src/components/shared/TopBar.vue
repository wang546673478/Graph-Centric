<script setup lang="ts">
import DetailModeToggle from './DetailModeToggle.vue'
import StatusPill from './StatusPill.vue'
</script>

<template>
  <header class="topbar">
    <nav>
      <router-link to="/">Run</router-link>
      <router-link to="/runs">History</router-link>
      <router-link to="/skills">Skills</router-link>
      <router-link to="/files">Files</router-link>
      <router-link to="/settings">Settings</router-link>
    </nav>
    <div class="right">
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
</style>
