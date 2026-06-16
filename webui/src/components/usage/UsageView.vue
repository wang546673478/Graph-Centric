<script setup lang="ts">
import { ref, onMounted, computed } from 'vue'
import { api } from '../../composables/useRunSocket'

interface RunUsage {
  id: string; task: string; status: string; tokens: number; duration_ms: number
}

interface UsageStats {
  total_tokens: number; total_runs: number
  model_breakdown: Record<string, { calls: number; tokens: number }>
  runs: RunUsage[]
}

const stats = ref<UsageStats | null>(null)

onMounted(async () => { try { stats.value = await api.get('/api/usage') } catch { /* */ } })

const sortedRuns = computed(() =>
  [...(stats.value?.runs || [])].sort((a, b) => b.duration_ms - a.duration_ms)
)

function fmtTokens(n: number) {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + 'M'
  if (n >= 1_000) return (n / 1_000).toFixed(1) + 'K'
  return String(n)
}

function modelKeys(obj: Record<string, any>) {
  return Object.keys(obj)
}
</script>

<template>
  <div class="usage-page">
    <h2>Usage Statistics</h2>

    <div v-if="!stats" class="loading">Loading…</div>
    <template v-else>
      <!-- Summary cards -->
      <div class="cards">
        <div class="card">
          <div class="card-value">{{ stats.total_runs }}</div>
          <div class="card-label">Total Runs</div>
        </div>
        <div class="card">
          <div class="card-value">{{ fmtTokens(stats.total_tokens) }}</div>
          <div class="card-label">Total Tokens</div>
        </div>
        <div class="card" v-for="(v, k) in stats.model_breakdown" :key="k">
          <div class="card-value small">{{ k.slice(0, 40) }}</div>
          <div class="card-label">{{ fmtTokens(v.tokens) }} tokens</div>
        </div>
      </div>

      <!-- Run table -->
      <section>
        <h3>Run Detail</h3>
        <table class="run-table">
          <thead><tr><th>ID</th><th>Task</th><th>Status</th><th>Tokens</th><th>Duration</th></tr></thead>
          <tbody>
            <tr v-for="r in sortedRuns" :key="r.id">
              <td class="mono">{{ r.id.slice(0, 8) }}…</td>
              <td>{{ r.task?.slice(0, 50) || '(untitled)' }}</td>
              <td><span class="status-pill" :class="r.status.toLowerCase()">{{ r.status }}</span></td>
              <td>{{ fmtTokens(r.tokens) }}</td>
              <td>{{ (r.duration_ms / 1000).toFixed(1) }}s</td>
            </tr>
          </tbody>
        </table>
      </section>
    </template>
  </div>
</template>

<style scoped>
.usage-page { padding: 32px; max-width: 960px; }
h2 { font-size: 1.3rem; font-weight: 700; margin-bottom: 24px; }
h3 { font-size: 0.8rem; text-transform: uppercase; color: var(--text-muted); letter-spacing: 0.05em; margin: 24px 0 12px; }
.loading { color: var(--text-muted); }
.cards { display: flex; gap: 12px; flex-wrap: wrap; }
.card {
  background: var(--bg-panel); border: 1px solid var(--border);
  border-radius: var(--radius); padding: 16px 20px;
  box-shadow: var(--shadow); min-width: 140px;
}
.card-value { font-size: 1.4rem; font-weight: 700; color: var(--accent); }
.card-value.small { font-size: 0.75rem; font-family: var(--font-mono); }
.card-label { font-size: 0.7rem; color: var(--text-muted); margin-top: 4px; }
.run-table { width: 100%; border-collapse: collapse; margin-top: 8px; }
.run-table th { text-align: left; padding: 8px 12px; font-size: 0.65rem; text-transform: uppercase; color: var(--text-muted); letter-spacing: 0.08em; border-bottom: 1px solid var(--border); }
.run-table td { padding: 10px 12px; font-size: 0.82rem; border-bottom: 1px solid var(--border); }
.mono { font-family: var(--font-mono); font-size: 0.75rem !important; color: var(--text-muted); }
</style>
