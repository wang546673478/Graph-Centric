import { ref, computed, reactive } from 'vue'
import en from '../locales/en'
import zhCN from '../locales/zh-CN'

export type Locale = 'en' | 'zh-CN'

const messages: Record<Locale, typeof en> = { en, 'zh-CN': zhCN as any }

export const locale = ref<Locale>((navigator.language?.startsWith('zh') ? 'zh-CN' : 'en') as Locale)

export function useI18n() {
  function t(key: string): string {
    const keys = key.split('.')
    let result: any = messages[locale.value]
    for (const k of keys) {
      result = result?.[k]
    }
    return typeof result === 'string' ? result : key
  }

  function setLocale(l: Locale) {
    locale.value = l
  }

  const isZh = computed(() => locale.value === 'zh-CN')

  return { t, locale, setLocale, isZh }
}
