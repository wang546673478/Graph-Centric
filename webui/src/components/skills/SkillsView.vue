<script setup lang="ts">import { useI18n } from '../../composables/useI18n'; const { t } = useI18n()</script>
<template><div class="skills"><h2>{{ t('skills.title') }}</h2><p class="muted">{{ t('skills.empty') }}</p></div></template>
<style scoped>.skills{padding:24px}.muted{color:var(--text-muted)}</style>
