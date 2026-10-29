import type { ConfigImport } from '../types'
import { t } from './i18n'

/** 导入配置后的胶囊通知，按丢弃的情况拼成一句 */
export function importMessage(r: ConfigImport) {
  const parts = [
    r.missing ? t('{n} 个文件夹不存在已跳过', { n: r.missing }) : '',
    r.inaccessible ? t('{n} 个文件夹无法访问已跳过', { n: r.inaccessible }) : '',
    r.failed ? t('{n} 个文件夹未能添加', { n: r.failed }) : '',
    r.droppedParams ? t('{n} 项参数无效已跳过', { n: r.droppedParams }) : '',
  ].filter(Boolean)
  return [t('配置已导入'), ...parts].join(t('，'))
}
