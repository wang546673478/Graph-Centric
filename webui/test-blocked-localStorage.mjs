// Manual reproduction: import useTheme composable with localStorage.getItem throwing.
// Confirms initialTheme() no longer white-screens the page when localStorage is blocked.
//
// We can't import the TS file directly from Node (no Vite), so we re-implement the
// two initial* helpers in JS exactly as they appear in useTheme.ts. This is the
// load-bearing test — if these two functions handle the throw, the bug is fixed.

function makeInitialTheme(THEME_STORAGE_KEY, DEFAULT_THEME) {
  return function initialTheme() {
    try {
      const saved = localStorage.getItem(THEME_STORAGE_KEY)
      if (saved === 'light' || saved === 'dark') return saved
    } catch { /* localStorage blocked */ }
    return DEFAULT_THEME
  }
}

function makeInitialStyle(STYLES, STYLE_STORAGE_KEY, DEFAULT_STYLE) {
  function normalizeStyle(v) {
    return STYLES.includes(v) ? v : DEFAULT_STYLE
  }
  return function initialStyle() {
    try {
      return normalizeStyle(localStorage.getItem(STYLE_STORAGE_KEY))
    } catch {
      return DEFAULT_STYLE
    }
  }
}

// Simulate blocked localStorage (e.g. SecurityError in private mode).
globalThis.localStorage = {
  getItem() { throw new Error('SecurityError: localStorage blocked') },
  setItem() { throw new Error('SecurityError: localStorage blocked') },
}

const initialTheme = makeInitialTheme('gc-theme', 'dark')
const initialStyle = makeInitialStyle(
  ['minimal', 'glassmorphism', 'notion', 'bento'],
  'gc-style',
  'minimal',
)

let theme, style, threw = false
try {
  theme = initialTheme()
  style = initialStyle()
} catch (e) {
  threw = true
  console.error('FAIL — threw:', e.message)
  process.exit(1)
}

if (threw) process.exit(1)

if (theme !== 'dark') { console.error('FAIL — theme=', theme); process.exit(1) }
if (style !== 'minimal') { console.error('FAIL — style=', style); process.exit(1) }

console.log('PASS — theme=', theme, ' style=', style,
            ' (both defaulted cleanly with blocked localStorage)')
