<script setup lang="ts">import { useI18n } from '../../composables/useI18n'; const { t } = useI18n()</script>
<template><div class="files"><h2>{{ t('files.title') }}</h2><p class="muted">{{ t('files.empty') }}</p></div></template>
<style scoped>.files{padding:24px}.muted{color:var(--text-muted)}</style>
