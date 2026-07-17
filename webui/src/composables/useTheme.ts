import { ref, watch } from 'vue'

/* ------------------------------ data-theme axis ----------------------------- */

export type Theme = 'light' | 'dark'

const THEME_STORAGE_KEY = 'gc-theme'
const DEFAULT_THEME: Theme = 'dark'

function initialTheme(): Theme {
  try {
    const saved = localStorage.getItem(THEME_STORAGE_KEY)
    if (saved === 'light' || saved === 'dark') return saved
  } catch { /* localStorage blocked — fall through to default */ }
  return DEFAULT_THEME
}

export const theme = ref<Theme>(initialTheme())

/** 把当前主题写到 <html data-theme>。在 mount 前调用一次,避免首帧闪白。 */
export function applyTheme(t: Theme = theme.value) {
  document.documentElement.setAttribute('data-theme', t)
}

watch(theme, (t) => {
  applyTheme(t)
  try { localStorage.setItem(THEME_STORAGE_KEY, t) } catch { /* */ }
})

/* ------------------------------ data-style axis ----------------------------- */

export type StyleId = 'minimal' | 'glassmorphism' | 'notion' | 'bento'
export const STYLES: StyleId[] = ['minimal', 'glassmorphism', 'notion', 'bento']

const STYLE_STORAGE_KEY = 'gc-style'
const DEFAULT_STYLE: StyleId = 'minimal'

function normalizeStyle(v: string | null): StyleId {
  return (STYLES as string[]).includes(v ?? '') ? (v as StyleId) : DEFAULT_STYLE
}

function initialStyle(): StyleId {
  try {
    return normalizeStyle(localStorage.getItem(STYLE_STORAGE_KEY))
  } catch {
    return DEFAULT_STYLE
  }
}

export const style = ref<StyleId>(initialStyle())

/** 把当前风格写到 <html data-style>。在 mount 前调用一次,避免首帧默认态闪现。 */
export function applyStyle(s: StyleId = style.value) {
  document.documentElement.setAttribute('data-style', s)
}

watch(style, (s) => {
  applyStyle(s)
  try { localStorage.setItem(STYLE_STORAGE_KEY, s) } catch { /* */ }
})

/* ---------------------------------- useTheme -------------------------------- */

export function useTheme() {
  function toggleTheme() {
    theme.value = theme.value === 'dark' ? 'light' : 'dark'
  }
  function setStyle(s: StyleId) {
    if (!(STYLES as string[]).includes(s)) return
    style.value = s
  }
  return { theme, style, toggleTheme, setStyle, STYLES }
}