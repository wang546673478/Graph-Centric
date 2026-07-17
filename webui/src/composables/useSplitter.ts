import { ref, watch, type Ref } from 'vue'

export interface SplitterOptions {
  storageKey: string
  initial: number   // 初始像素宽
  min: number
  max: number
}

/**
 * 拖拽调宽。返回受控的 `size`(px)和一个 `startDrag(e)` 供分隔条
 * mousedown 调用。`fromRight=true` 时拖动方向反向(用于右栏:向左拖变宽)。
 */
export function useSplitter(opts: SplitterOptions, fromRight = false): {
  size: Ref<number>
  startDrag: (e: MouseEvent) => void
} {
  // localStorage can throw SecurityError in private mode / iframe sandbox;
  // the read runs at composable setup so an unprotected call would break the
  // caller. Wrap defensively and fall back to the supplied initial size.
  let saved = NaN
  try {
    saved = Number(localStorage.getItem(opts.storageKey))
  } catch { /* fall through to initial */ }
  const size = ref(Number.isFinite(saved) && saved > 0 ? saved : opts.initial)
  watch(size, (v) => {
    try { localStorage.setItem(opts.storageKey, String(Math.round(v))) } catch { /* blocked */ }
  })

  function startDrag(e: MouseEvent) {
    e.preventDefault()
    const startX = e.clientX
    const startSize = size.value
    function onMove(ev: MouseEvent) {
      const dx = ev.clientX - startX
      const next = startSize + (fromRight ? -dx : dx)
      size.value = Math.max(opts.min, Math.min(opts.max, next))
    }
    function onUp() {
      window.removeEventListener('mousemove', onMove)
      window.removeEventListener('mouseup', onUp)
      document.body.style.userSelect = ''
      document.body.style.cursor = ''
    }
    document.body.style.userSelect = 'none'
    document.body.style.cursor = 'col-resize'
    window.addEventListener('mousemove', onMove)
    window.addEventListener('mouseup', onUp)
  }

  return { size, startDrag }
}
