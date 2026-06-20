import { ref, watch } from 'vue'

export type Theme = 'light' | 'dark'

const STORAGE_KEY = 'gc-theme'

function initialTheme(): Theme {
  const saved = localStorage.getItem(STORAGE_KEY)
  if (saved === 'light' || saved === 'dark') return saved
  return 'dark' // 默认深色
}

// 模块级单例,全应用共享同一份主题状态。
export const theme = ref<Theme>(initialTheme())

/** 把当前主题写到 <html data-theme>。在 mount 前调用一次,避免首帧闪白。 */
export function applyTheme(t: Theme = theme.value) {
  document.documentElement.setAttribute('data-theme', t)
}

// 主题变化时同步 DOM + localStorage。
watch(theme, (t) => {
  applyTheme(t)
  localStorage.setItem(STORAGE_KEY, t)
})

export function useTheme() {
  function toggleTheme() {
    theme.value = theme.value === 'dark' ? 'light' : 'dark'
  }
  return { theme, toggleTheme }
}
