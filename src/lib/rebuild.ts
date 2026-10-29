import type { RebuildStatus } from '../types'
import { KIND_LABELS } from './format'
import { t } from './i18n'

// 按新设置重建的提示文字

/** 各种类的计数文字（词典键） */
const COUNTS: Record<string, string> = { image: '{n} 张图片', document: '{n} 个文档', audio: '{n} 个音频', video: '{n} 个视频' }

/** 还要重建的文件，如“1,234 张图片、56 个视频” */
export function rebuildCounts(byKind: Record<string, number>) {
  return Object.keys(KIND_LABELS)
    .filter((k) => byKind[k] > 0)
    .map((k) => t(COUNTS[k], { n: byKind[k].toLocaleString('en-US') }))
    .join(t('、'))
}

/** 设置页的提示 */
export function rebuildNotice(r: RebuildStatus) {
  return t('按新设置还需重建 {counts}，期间旧索引仍可搜索。', { counts: rebuildCounts(r.byKind) })
}
