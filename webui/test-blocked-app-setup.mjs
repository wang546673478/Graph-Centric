// Manual reproduction: call App.vue's localStorage helpers with
// localStorage.getItem + setItem throwing, to confirm App.vue's
// <script setup> no longer white-screens the page when localStorage
// is blocked.
//
// We can't import the .vue file directly from Node (no Vite runtime),
// so we re-implement the helpers exactly as they appear in App.vue.
// This is the load-bearing test — if these helpers handle the throw,
// the App setup does too. Keep in lock-step with App.vue.

function readBool(key, fallback) {
  try { return localStorage.getItem(key) === '1' } catch { return fallback }
}
function readEq(key, sentinel, fallback) {
  try {
    const v = localStorage.getItem(key)
    if (v === null) return sentinel === null
    return v === '1'
  } catch { return fallback }
}
function writeBool(key, v) {
  try { localStorage.setItem(key, v ? '1' : '0') } catch { /* blocked */ }
}

// Simulate blocked localStorage (e.g. SecurityError in private mode).
globalThis.localStorage = {
  getItem() { throw new Error('SecurityError: localStorage blocked') },
  setItem() { throw new Error('SecurityStorage: localStorage blocked') },
}

const SIDEBAR_KEY = 'gc-sidebar-collapsed'
const RIGHT_KEY = 'gc-right-collapsed'

let sidebarCollapsed, rightCollapsed, threw = false
try {
  // Mirrors the two ref() initializers in App.vue.
  sidebarCollapsed = readBool(SIDEBAR_KEY, false)
  rightCollapsed = readEq(RIGHT_KEY, null, true)
  // Mirrors the two watcher callbacks in App.vue. They fire async, so
  // verify writes also don't throw.
  writeBool(SIDEBAR_KEY, sidebarCollapsed)
  writeBool(RIGHT_KEY, rightCollapsed)
} catch (e) {
  threw = true
  console.error('FAIL — threw:', e.message)
  process.exit(1)
}

if (threw) process.exit(1)

if (sidebarCollapsed !== false) {
  console.error('FAIL — sidebarCollapsed=', sidebarCollapsed, '(expected false)')
  process.exit(1)
}
if (rightCollapsed !== true) {
  console.error('FAIL — rightCollapsed=', rightCollapsed, '(expected true, "first-load default")')
  process.exit(1)
}

console.log('PASS — sidebarCollapsed=', sidebarCollapsed,
            ' rightCollapsed=', rightCollapsed,
            ' (both defaulted cleanly with blocked localStorage)')
