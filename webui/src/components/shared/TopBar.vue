<template>
  <header class="topbar">
    <nav><router-link to="/">Run</router-link> <router-link to="/runs">Runs</router-link> <router-link to="/skills">Skills</router-link> <router-link to="/files">Files</router-link> <router-link to="/settings">Settings</router-link></nav>
    <div class="right"><DetailModeToggle /><StatusPill /></div>
  </header>
</template>
<script setup lang="ts">import DetailModeToggle from './DetailModeToggle.vue'; import StatusPill from './StatusPill.vue'</script>
<style scoped>.topbar{display:flex;align-items:center;justify-content:space-between;padding:8px 16px;background:var(--bg-panel);border-bottom:1px solid var(--border);min-height:44px}nav a{padding:4px 10px;color:var(--text-muted);text-decoration:none;border-radius:4px;font-size:.8rem}nav a:hover,nav a.router-link-active{color:var(--text);background:var(--bg-hover)}.right{display:flex;gap:8px;align-items:center}</style>