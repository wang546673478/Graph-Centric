<script setup lang="ts">import { useI18n } from '../../composables/useI18n'; const { t } = useI18n()</script>
<template><div class="history"><h2>{{ t('history.title') }}</h2><p class="muted">{{ t('history.empty') }}</p></div></template>
<style scoped>.history{padding:24px}.muted{color:var(--text-muted)}</style>
