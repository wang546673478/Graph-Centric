import { ref, reactive, onUnmounted } from 'vue'

// ---- types ----
export interface WSEvent {
  type: string; data?: any
  role?: string; content?: string
  nodes?: any[]; edges?: any[]
  phase?: string; message?: string; tokens_used?: number
  kind?: string; payload?: any
  verdict?: string; rationale?: string
  index?: number; round?: number; node_count?: number; edge_count?: number
  changed_node?: string; predecessor?: string; depth?: number
  component?: string; model_name?: string; tier?: string
  request_preview?: string; response_content?: string
  finish_reason?: string; prompt_tokens?: number; completion_tokens?: number; duration_ms?: number
}

export interface RunData {
  id: string; task: string; status: string
  duration_sec: number; tokensUsed: number
  transcript: { role: string; content: string }[]
  nodes: any[]; edges: any[]
  error: string | null
}

// ---- global state (survives navigation) ----
export const runs = ref<RunData[]>([])
export const activeRunId = ref<string | null>(null)
export const detailMode = ref(false)

// Per-run reactive data store — keyed by run ID.
const runStores = new Map<string, {
  transcript: { role: string; content: string }[]
  nodes: any[]; edges: any[]
  status: string; tokensUsed: number; error: string
}>()

function getStore(id: string) {
  if (!runStores.has(id)) {
    runStores.set(id, reactive({
      transcript: [] as { role: string; content: string }[],
      nodes: [] as any[], edges: [] as any[],
      status: 'idle', tokensUsed: 0, error: '',
    }))
  }
  return runStores.get(id)!
}

export function getRunStore(id: string) {
  return getStore(id)
}

// ---- WS connection ----
export function useRunSocket(runId: string, onEvent: (e: WSEvent) => void) {
  const connected = ref(false)
  let ws: WebSocket | null = null
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null
  let backoff = 1000

  function connect() {
    const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
    ws = new WebSocket(`${protocol}//${location.host}/ws/runs/${runId}`)
    ws.onopen = () => { connected.value = true; backoff = 1000 }
    ws.onmessage = (msg) => {
      try { onEvent(JSON.parse(msg.data)) } catch { /* skip */ }
    }
    ws.onclose = () => {
      connected.value = false
      reconnectTimer = setTimeout(() => { backoff = Math.min(backoff * 2, 30000); connect() }, backoff)
    }
    ws.onerror = () => ws?.close()
  }

  function send(msg: Record<string, any>) {
    if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(msg))
  }

  function disconnect() { if (reconnectTimer) clearTimeout(reconnectTimer); ws?.close() }

  onUnmounted(disconnect)
  connect()
  return { connected, send, disconnect }
}

// ---- API helpers ----
export const api = {
  get: (path: string) => fetch(path).then(r => r.json()),
  post: (path: string, body?: any) =>
    fetch(path, { method: 'POST', headers: { 'content-type': 'application/json' }, body: body ? JSON.stringify(body) : undefined }).then(r => r.json()),
  del: (path: string) => fetch(path, { method: 'DELETE' }).then(r => r.json()),
}

// ---- run management ----
export function createRun(task: string): Promise<{ id: string }> {
  return api.post('/api/runs', { task })
}

export async function loadRuns() {
  const raw = await api.get('/api/runs')
  runs.value = raw.map((r: any) => {
    const id = r.id
    const existing = runStores.get(id)
    return {
      id, task: r.task, status: statusLabel(r.status),
      duration_sec: Math.round((r.duration_ms || 0) / 1000),
      tokensUsed: existing?.tokensUsed || 0,
      transcript: existing?.transcript || [],
      nodes: existing?.nodes || [],
      edges: existing?.edges || [],
      error: existing?.error || null,
    }
  })
}

export function findRun(id: string): RunData | undefined {
  return runs.value.find(r => r.id === id)
}

/// Fetch checkpoint data for a completed run and populate the global store.
export async function loadRunData(id: string) {
  const store = getRunStore(id)
  try {
    const checkpoints: any[] = await api.get(`/api/runs/${id}/checkpoints`)
    if (checkpoints.length > 0) {
      const last = checkpoints[checkpoints.length - 1]
      const cp: any = await api.get(`/api/runs/${id}/checkpoints/${last.index}`)
      if (cp.transcript) store.transcript = cp.transcript.map((m: any) => {
        const role = typeof m.role === 'string' ? m.role.toLowerCase().replace(/^"|"$/g, '') : 'assistant'
        return { role, content: m.content }
      })
      if (cp.graph) {
        store.nodes = cp.graph.nodes ? Object.values(cp.graph.nodes) : []
        store.edges = cp.graph.edges || []
      }
      store.status = 'Done'
    }
  } catch { /* checkpoints may not exist */ }
  // Also load from active runs list for status.
  const r = findRun(id)
  if (r) { store.status = r.status; store.tokensUsed = r.tokensUsed }
}

function statusLabel(s: any): string {
  if (!s) return 'unknown'
  if (typeof s === 'string') return s.toLowerCase()
  const keys = Object.keys(s)
  const k = keys[0]?.toLowerCase() || 'unknown'
  if (k === 'error') return 'Error'
  if (k === 'running') return 'Running'
  if (k === 'paused') return 'Paused'
  if (k === 'done') return 'Done'
  if (k === 'cancelled') return 'Cancelled'
  if (k === 'graphinvalid') return 'GraphInvalid'
  return k
}

export function toggleDetailMode() {
  detailMode.value = !detailMode.value
}
