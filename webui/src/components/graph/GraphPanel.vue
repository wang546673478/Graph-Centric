<template><div class="graph-panel" ref="container"></div></template>
<script setup lang="ts">import { ref } from 'vue'; const container = ref<HTMLElement>()</script>
<style scoped>.graph-panel{flex:1;min-height:200px}</style>