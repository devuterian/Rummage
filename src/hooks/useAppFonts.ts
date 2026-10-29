import { useCallback, useEffect, useLayoutEffect, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import type { Appearance } from '../types'
import { applyAppearance, loadCachedAppearance, saveCachedAppearance } from '../lib/fonts'
import { getLang } from '../lib/i18n'
import { isTauri } from '../lib/platform'

/**
 * 外观（字体与界面语言）：先用本地缓存，首次绘制前就应用；核心连上后取 `get_appearance`，以核心为准并更新缓存。
 * 核心中还没有语言时保存启动时选定的语言（`main.tsx`）；与当前语言不同时重新加载页面。
 * `update` 立即应用并写入缓存与核心，保存失败时抛出，由设置页显示原因；改了语言时保存后重新加载页面
 */
export function useAppFonts(connected: boolean) {
  const [appearance, setAppearance] = useState(loadCachedAppearance)

  // 用 layout effect 在首次绘制前应用字体，避免先用默认字体再切换
  useLayoutEffect(() => applyAppearance(appearance), [appearance])

  useEffect(() => {
    if (!isTauri || !connected) return
    invoke<Appearance>('get_appearance')
      .then((a) => {
        const next = a.language ? a : { ...a, language: getLang() }
        setAppearance(next)
        saveCachedAppearance(next)
        if (!a.language) return invoke('set_appearance', { appearance: next })
        if (a.language !== getLang()) location.reload()
      })
      .catch((err) => console.warn('无法读取外观设置：', err))
  }, [connected])

  const update = useCallback(async (next: Appearance) => {
    setAppearance(next)
    saveCachedAppearance(next)
    await invoke('set_appearance', { appearance: next })
    if (next.language && next.language !== getLang()) location.reload()
  }, [])

  return { appearance, update }
}
