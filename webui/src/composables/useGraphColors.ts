import { reactive, watch } from 'vue'
import { theme } from './useTheme'

export interface GraphColors {
  node: string      // 普通节点(accent)
  complex: string   // 可钻取节点环(warning)
  scope: string     // in-scope(success)
  text: string      // 标签文字
  edge: string      // 普通边
  edgeScope: string // in-scope 边
  bg: string        // 画布/3D 背景
  grid: string      // 3D 网格/边框
}

function readVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim()
  return v || fallback
}

function compute(): GraphColors {
  return {
    node: readVar('--accent', '#7c3aed'),
    complex: readVar('--warning', '#d97706'),
    scope: readVar('--success', '#059669'),
    text: readVar('--text', '#1a1a2e'),
    edge: readVar('--border', '#c4b5e0'),
    edgeScope: readVar('--success', '#059669'),
    bg: readVar('--bg', '#f5f5f0'),
    grid: readVar('--border', '#e0ddd6'),
  }
}

/**
 * 主题感知的图配色。返回一个 reactive 对象,主题切换时其字段自动更新。
 * 调用方可 `watch(() => theme.value, ...)` 触发重渲染,或直接读取最新值。
 */
export function useGraphColors(): GraphColors {
  const colors = reactive(compute())
  watch(theme, () => Object.assign(colors, compute()))
  return colors
}
