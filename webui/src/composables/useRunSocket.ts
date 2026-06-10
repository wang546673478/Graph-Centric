import { ref, onUnmounted } from 'vue'

export interface WSEvent {
  type: string
  data: any
}

export function useRunSocket(runId: string) {
  const events = ref<WSEvent[]>([])
  const detailMode = ref(false)
  const connected = ref(false)
  let ws: WebSocket | null = null
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null
  let backoff = 1000

  function connect() {
    const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
    const url = `${protocol}//${location.host}/ws/runs/${runId}`
    ws = new WebSocket(url)
    ws.onopen = () => { connected.value = true; backoff = 1000 }
    ws.onmessage = (msg) => {
      try { events.value.push(JSON.parse(msg.data)) } catch { /* ignore */ }
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

  function toggleDetailMode() {
    detailMode.value = !detailMode.value
    send({ type: 'set_detail_mode', enabled: detailMode.value })
  }

  onUnmounted(() => { if (reconnectTimer) clearTimeout(reconnectTimer); ws?.close() })
  connect()
  return { events, detailMode, toggleDetailMode, connected, send }
}

export function apiGet(path: string) { return fetch(path).then(r => r.json()) }
export function apiPost(path: string, body: any) { return fetch(path, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) }).then(r => r.json()) }
