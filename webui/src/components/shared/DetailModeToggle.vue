<template><label class="toggle"><input type="checkbox" v-model="on" @change="$emit('toggle', on)" /> <span>Detail</span></label></template>
<script setup lang="ts">import { ref } from 'vue'; const on = ref(false); defineEmits(['toggle'])</script>
<style scoped>.toggle{display:flex;align-items:center;gap:4px;font-size:.75rem;color:var(--text-muted);cursor:pointer}</style>