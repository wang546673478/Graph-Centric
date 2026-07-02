import { ref, reactive, onUnmounted } from 'vue'

// ---- types ----
export interface WSEvent {
  type: string; data?: any
  role?: string; content?: string
  nodes?: any[]; edges?: any[]
  added_nodes?: any[]; removed_node_ids?: string[]
  added_edges?: any[]; removed_edges?: any[]; replaced?: boolean
  phase?: string; message?: string; tokens_used?: number
  kind?: string; payload?: any; question?: string; options?: string[]
  verdict?: string; rationale?: string
  index?: number; round?: number; node_count?: number; edge_count?: number
  changed_node?: string; predecessor?: string; depth?: number
  component?: string; model_name?: string; tier?: string
  request_preview?: string; response_content?: string; reasoning_content?: string
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
  round: number; lastCheckpoint: number
  // v2 spec §4.2: phase-progress data fed by the `graph_phase` WS
  // event. Components read this to render the top status bar.
  phaseProgress: {
    graph_phase: string
    round: number
    clarification_count: number
    explorer_iter: number
    graph_version: number
    ts: number
  } | null
}>()

function getStore(id: string) {
  if (!runStores.has(id)) {
    runStores.set(id, reactive({
      transcript: [] as { role: string; content: string }[],
      nodes: [] as any[], edges: [] as any[],
      status: 'idle', tokensUsed: 0, error: '',
      round: 0, lastCheckpoint: -1,
      phaseProgress: null,
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
  // v2 spec §4.7: monotonic event counter tracked by the
  // frontend. Used to detect "missed events" on reconnect.
  let lastEventId = 0
  let ws: WebSocket | null = null
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null
  let backoff = 1000
  const MAX_BACKOFF_MS = 30000
  const MAX_RECONNECTS = 20
  let reconnectAttempts = 0

  function connect() {
    const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
    ws = new WebSocket(`${protocol}//${location.host}/ws/runs/${runId}`)
    ws.onopen = () => {
      connected.value = true
      backoff = 1000
      reconnectAttempts = 0
    }
    ws.onmessage = (msg) => {
      try {
        const parsed = JSON.parse(msg.data)
        // Backend stamps every event with an `id` field for the
        // Last-Event-ID protocol. If the gap between the last
        // seen id and the new id is > 1, we missed events; the
        // UI can show a "reconnect — events may have been
        // missed" hint.
        if (typeof parsed.id === 'number') {
          if (lastEventId > 0 && parsed.id > lastEventId + 1) {
            // Surface a synthetic "missed_events" notice so the
            // UI can show a banner. The actual missed events
            // are not replayed (no event log on the server yet);
            // the user can re-checkpoint to recover.
            onEvent({
              type: 'missed_events',
              data: {
                from_id: lastEventId,
                to_id: parsed.id,
                count: parsed.id - lastEventId - 1,
              },
            })
          }
          lastEventId = parsed.id
        }
        onEvent(parsed)
      } catch {
        /* skip */
      }
    }
    ws.onclose = () => {
      connected.value = false
      reconnectAttempts += 1
      if (reconnectAttempts > MAX_RECONNECTS) {
        // v2 spec §4.7: surface a "connection lost" UI
        // state. Caller can show a banner with a "retry
        // now" button.
        onEvent({
          type: 'connection_lost',
          data: { attempts: reconnectAttempts, run_id: runId },
        })
        return
      }
      // Exponential backoff with full jitter (50%-100% of the
      // computed delay) to avoid thundering herd when many
      // clients reconnect at once.
      const delay = Math.floor(backoff * (0.5 + Math.random() * 0.5))
      backoff = Math.min(backoff * 2, MAX_BACKOFF_MS)
      reconnectTimer = setTimeout(() => connect(), delay)
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
//
// `fetch` doesn't reject on 4xx/5xx — it resolves with a Response
// whose `.ok` is false. Wrap the helpers so callers can `try/catch`
// the way the standard fetch API behaves.
export const api = {
  get: async (path: string) => {
    const r = await fetch(path)
    if (!r.ok) throw new Error(`${r.status} ${r.statusText}: ${await r.text()}`)
    return r.json()
  },
  post: async (path: string, body?: any) => {
    const r = await fetch(path, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: body ? JSON.stringify(body) : undefined,
    })
    if (!r.ok) {
      // Try to surface a useful error message — backend wraps
      // errors in `{"error": "..."}` (see ApiError's Serialize impl).
      const text = await r.text()
      let detail = text
      try { detail = (JSON.parse(text) as any).error ?? text } catch { /* keep raw */ }
      throw new Error(`${r.status} ${r.statusText}: ${detail}`)
    }
    return r.json()
  },
  del: async (path: string) => {
    const r = await fetch(path, { method: 'DELETE' })
    if (!r.ok) throw new Error(`${r.status} ${r.statusText}: ${await r.text()}`)
    return r.json()
  },
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
