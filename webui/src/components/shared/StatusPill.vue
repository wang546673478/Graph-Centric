<template><span class="status-pill" :class="status">{{ status }}</span></template>
<script setup lang="ts">import { ref } from 'vue'; const status = ref('idle')</script>