import type { Lang } from '../types'

/**
 * 界面语言：中文原文就是键，韩语、英语的词典按原文查找，查不到时显示原文。
 * 词典按界面区域拆成 `src/locales/<语言>/*.ts`，每个文件默认导出 `{ 原文: 译文 }`，这里合并。
 * 插值写成 `{name}`：`t('已索引 {n} 个文件', { n })`。
 * 未设置时为中文（测试按中文断言）；启动时由 `main.tsx` 按缓存的外观设置或系统语言设置，切换语言后重新加载页面
 */
type Dict = Record<string, string>

function merge(mods: Record<string, Dict>): Dict {
  return Object.assign({}, ...Object.values(mods))
}

const DICTS: Record<Exclude<Lang, 'zh'>, Dict> = {
  ko: merge(import.meta.glob<Dict>('../locales/ko/*.ts', { eager: true, import: 'default' })),
  en: merge(import.meta.glob<Dict>('../locales/en/*.ts', { eager: true, import: 'default' })),
}

export const LANGS: Lang[] = ['ko', 'zh', 'en']

/** 语言选择中各语言用自己的写法显示 */
export const LANG_NAMES: Record<Lang, string> = { ko: '한국어', zh: '中文', en: 'English' }

let current: Lang = 'zh'

export function getLang() {
  return current
}

export function setLang(lang: Lang) {
  current = lang
  document.documentElement.lang = lang === 'zh' ? 'zh-CN' : lang
}

/** 系统语言是中文、英语时用对应语言，其余用韩语 */
export function detectLang(): Lang {
  const sys = (navigator.language || '').toLowerCase()
  if (sys.startsWith('zh')) return 'zh'
  if (sys.startsWith('en')) return 'en'
  return 'ko'
}

export function isLang(v: unknown): v is Lang {
  return LANGS.includes(v as Lang)
}

export function t(zh: string, vars?: Record<string, string | number>) {
  const s = current === 'zh' ? zh : (DICTS[current][zh] ?? zh)
  if (!vars) return s
  return s.replace(/\{(\w+)\}/g, (m, k: string) => (k in vars ? String(vars[k]) : m))
}
