/** i18next 翻译函数的最小签名（用 unknown 保持与 useTranslation 的 t 宽兼容）。 */
export type TranslateFn = (key: string, options?: unknown) => string;

/**
 * 相对时间格式化工具：把时间戳渲染成"x 分钟前 / x 小时前 / 昨天 / x 天前"等自然语言。
 * 文案走 i18n（由调用方传入 t，中英文各 bucket 的 key 在 locales 里定义）；
 * 时间戳非法（NaN / 空串解析）时返回空串，让上层跳过显示。
 * 分档逻辑：
 *   < 60s        → 刚刚
 *   < 60min      → N 分钟前
 *   < 24h        → N 小时前
 *   == 1 天      → 昨天（对用户更自然）
 *   >= 2 天      → N 天前
 */
export function formatRelativeTime(ts: number, now: number, t: TranslateFn): string {
  if (!Number.isFinite(ts) || !Number.isFinite(now)) return '';
  const diffSec = Math.floor((now - ts) / 1000);
  // 未来时间（时钟偏差/并发写入）与刚发生一并以"刚刚"显示
  if (diffSec < 60) return t('agent.timeJustNow');
  const minutes = Math.floor(diffSec / 60);
  if (minutes < 60) return t('agent.timeMinutesAgo', { count: minutes });
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return t('agent.timeHoursAgo', { count: hours });
  const days = Math.floor(hours / 24);
  if (days === 1) return t('agent.timeYesterday');
  return t('agent.timeDaysAgo', { count: days });
}
