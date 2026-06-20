import { marked } from 'marked'

// 配置一次(模块级):GFM + 软换行转 <br>,贴合聊天气泡习惯。
marked.setOptions({
  gfm: true,
  breaks: true,
})

/**
 * 把 markdown 文本渲染为 HTML 字符串,供 v-html 使用。
 * marked 不执行脚本;输入按 markdown 解析,内嵌 raw HTML 不被特殊处理为可执行内容。
 * 对话内容来自模型,非用户可信输入也无妨——这里只做展示渲染,不注入到可执行上下文。
 */
export function renderMarkdown(text: string): string {
  if (!text) return ''
  try {
    return marked.parse(text, { async: false }) as string
  } catch {
    // 渲染失败时退回转义后的纯文本,避免破坏页面。
    const div = document.createElement('div')
    div.textContent = text
    return div.innerHTML
  }
}

export function useMarkdown() {
  return { renderMarkdown }
}
