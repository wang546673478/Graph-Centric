import { ref, reactive, onUnmounted, computed } from 'vue'

// ---- types ----
export interface WSEvent {
  type: string
  data?: any
  role?: string; content?: string
  nodes?: any[]; edges?: any[]
  phase?: string; message?: string; tokens_used?: number
  kind?: string; payload?: any
  verdict?: string; rationale?: string
  index?: number; round?: number; node_count?: number; edge_count?: number
  changed_node?: string; predecessor?: string; depth?: number
}

export interface RunState {
  id: string; task: string; status: string
  duration_sec: number; tokensUsed: number
  transcript: { role: string; content: string }[]
  nodes: any[]; edges: any[]
  error: string | null
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

// ---- global run list ----
export const runs = ref<RunState[]>([])
export const activeRunId = ref<string | null>(null)
export const detailMode = ref(false)

export function createRun(task: string): Promise<{ id: string }> {
  return api.post('/api/runs', { task })
}

export async function loadRuns() {
  const raw = await api.get('/api/runs')
  runs.value = raw.map((r: any) => ({
    id: r.id, task: r.task, status: statusLabel(r.status),
    duration_sec: Math.round((r.duration_ms || 0) / 1000),
    tokensUsed: 0, transcript: [], nodes: [], edges: [], error: null,
  }))
}

export function findRun(id: string) { return runs.value.find(r => r.id === id) }

function statusLabel(s: any): string {
  if (!s) return 'unknown'
  if (typeof s === 'string') return s.toLowerCase()
  // axum enum variants: {"Running":null} etc
  const keys = Object.keys(s)
  return keys[0]?.toLowerCase() || 'unknown'
}

export function toggleDetailMode() {
  detailMode.value = !detailMode.value
}

// ---- Cytoscape helpers ----
export function initCytoscape(container: HTMLElement, nodes: any[], edges: any[]) {
  const cy = (window as any).cytoscape({
    container,
    elements: [
      ...nodes.map((n: any) => ({ data: { id: n.id, label: n.summary || n.id } })),
      ...edges.map((e: any, i: number) => ({ data: { id: `e${i}`, source: e.source, target: e.target, label: e.relation } })),
    ],
    style: [
      { selector: 'node', style: { 'background-color': '#3b82f6', 'label': 'data(label)', 'color': '#e2e8f0', 'text-wrap': 'wrap', 'text-max-width': '120px', 'font-size': '9px' } },
      { selector: 'edge', style: { 'width': 1, 'line-color': '#64748b', 'target-arrow-color': '#64748b', 'target-arrow-shape': 'triangle', 'curve-style': 'bezier', 'label': 'data(label)', 'font-size': '7px', 'color': '#94a3b8' } },
    ],
    layout: { name: 'cose', animate: false, idealEdgeLength: 80, nodeRepulsion: 4000 },
  })
  return cy
}
