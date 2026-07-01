// Global keyboard shortcut handler.
// Subscribes to keydown on `window` for a small set of app-level
// shortcuts. Component-scoped shortcuts should stay in their own
// components (e.g. the composer's Enter-to-send).
import { onMounted, onUnmounted, ref } from 'vue'

export type Shortcut = {
  key: string
  /** Optional shift modifier. */
  shift?: boolean
  /** Optional meta/ctrl modifier (mapped automatically per platform). */
  meta?: boolean
  description: string
  run: () => void
}

const isMac = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform)

export function useShortcuts() {
  const showHelp = ref(false)

  const userShortcuts: Shortcut[] = []

  function install(extra: Shortcut[]) {
    userShortcuts.length = 0
    userShortcuts.push(...extra)
  }

  function onKey(e: KeyboardEvent) {
    // Don't intercept when the user is typing in a field.
    const t = e.target as HTMLElement | null
    if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) {
      // Only intercept Escape and ? inside inputs.
      if (e.key !== 'Escape' && e.key !== '?') return
    }
    // "/" focuses the search/composer (unless already typing).
    if (e.key === '/' && !(t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA'))) {
      e.preventDefault()
      // Prefer the composer (more common target). Fall back to sidebar search.
      const el = document.querySelector('.composer-input, .composer input, .sidebar .search') as HTMLElement | null
      el?.focus()
      return
    }
    // "?" anywhere opens the help modal.
    if (e.key === '?') {
      e.preventDefault()
      showHelp.value = !showHelp.value
      return
    }
    // Escape closes help.
    if (e.key === 'Escape' && showHelp.value) {
      e.preventDefault()
      showHelp.value = false
      return
    }
    // Component-registered shortcuts.
    for (const s of userShortcuts) {
      const modOk = s.meta ? (isMac ? e.metaKey : e.ctrlKey) : !(e.metaKey || e.ctrlKey)
      if (!modOk) continue
      if (e.shift !== !!s.shift) continue
      if (e.key.toLowerCase() !== s.key.toLowerCase()) continue
      e.preventDefault()
      s.run()
      return
    }
  }

  onMounted(() => { window.addEventListener('keydown', onKey) })
  onUnmounted(() => { window.removeEventListener('keydown', onKey) })

  return { showHelp, install, isMac }
}
