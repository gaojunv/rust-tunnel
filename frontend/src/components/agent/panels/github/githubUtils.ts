import { formatRelativeTime, type TranslateFn } from '../../formatRelativeTime';

/** 运行/作业状态：GitHub 原生 status 取值（queued/in_progress/waiting/requested/
 *  pending/completed 等；新取值未知时按原文透出）。 */
export type GhActivityStatus = string;

/** 运行/作业结论：GitHub 原生 conclusion 取值（success/failure/cancelled/skipped/
 *  timed_out 等；未完成时为 null）。 */
export type GhConclusion = string;

/** 仍在进行中（可被 cancel 拦截）的 status 集合。 */
const RUNNING_STATUSES: ReadonlySet<string> = new Set([
  'queued',
  'in_progress',
  'waiting',
  'requested',
  'pending',
]);

/** status → i18n 文案 key（agent.githubRunStatus_<status>）；未知状态回退原文。 */
export function runStatusLabel(status: string | undefined | null, t: TranslateFn): string {
  if (!status) return t('agent.githubStatusUnknown');
  const key = `agent.githubRunStatus_${status}`;
  return t(key) === key ? status : t(key);
}

/** conclusion → i18n 文案 key（agent.githubRunConclusion_<conclusion>）；未知回退原文。 */
export function conclusionLabel(
  conclusion: string | null | undefined,
  t: TranslateFn,
): string {
  if (!conclusion) return '';
  const key = `agent.githubRunConclusion_${conclusion}`;
  return t(key) === key ? conclusion : t(key);
}

/** 是否仍在进行中（queued/in_progress/waiting/requested/pending）。 */
export function isRunActive(status: string | undefined | null): boolean {
  return !!status && RUNNING_STATUSES.has(status);
}

/** 运行结论徽标样式：成功绿 / 失败红 / 其余中性。 */
export function conclusionBadgeClass(
  conclusion: string | null | undefined,
): string {
  if (conclusion === 'success') return 'bg-green-500/15 text-green-600 dark:text-green-400';
  if (conclusion === 'failure' || conclusion === 'timed_out')
    return 'bg-red-500/15 text-red-600 dark:text-red-400';
  return 'bg-muted text-muted-foreground';
}

/** GitHub ISO 时间串 → 相对时间文案（非法/空串 → 空串，上层跳过显示）。 */
export function formatGhTime(iso: string | undefined | null, t: TranslateFn): string {
  if (!iso) return '';
  return formatRelativeTime(Date.parse(iso), Date.now(), t);
}

/** 列表请求错误分类：429 限流 / 400 token 无效（后端 401→400）/ 502 上游错误 / 其他。 */
export type GithubErrorKind = 'ratelimit' | 'invalid-token' | 'upstream' | 'other';

export function githubErrorKind(err: unknown): GithubErrorKind {
  const status = (err as { response?: { status?: number } } | null)?.response?.status;
  if (status === 429) return 'ratelimit';
  if (status === 400) return 'invalid-token';
  if (status === 502) return 'upstream';
  return 'other';
}

/** 错误标题 i18n key（错误详情正文仍用后端 message，经 getApiErrorMessage 透出）。 */
export function githubErrorTitleKey(kind: GithubErrorKind): string {
  switch (kind) {
    case 'ratelimit':
      return 'agent.githubError_ratelimit';
    case 'invalid-token':
      return 'agent.githubError_invalid-token';
    case 'upstream':
      return 'agent.githubError_upstream';
    default:
      return 'agent.githubError_other';
  }
}

/** 解析 KV 输入行：跳过空 key，返回仅含有效键的对象。 */
export function serializeGhInputs(rows: { key: string; value: string }[]): Record<string, string> {
  const out: Record<string, string> = {};
  for (const row of rows) {
    const key = row.key.trim();
    if (key !== '') out[key] = row.value;
  }
  return out;
}

/** 耗时毫秒 → 紧凑可读串：1m32s / 58s / 2h5m。不足 1s 按 1s 计；非有限值返回空串。 */
export function formatDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return '';
  const totalSec = Math.max(1, Math.round(ms / 1000));
  const hours = Math.floor(totalSec / 3600);
  const minutes = Math.floor((totalSec % 3600) / 60);
  const seconds = totalSec % 60;
  if (hours > 0) {
    return minutes > 0 ? `${hours}h${minutes}m` : `${hours}h`;
  }
  if (minutes > 0) {
    return seconds > 0 ? `${minutes}m${seconds}s` : `${minutes}m`;
  }
  return `${seconds}s`;
}

/** 统一状态图标名（供 RunRow/JobStepRow 按名渲染对应 lucide 组件）。 */
export type GhStatusIconKind = 'success' | 'failure' | 'cancelled' | 'action_required' | 'active' | 'unknown';

/** 根据 status + conclusion 决定图标种类（priority：conclusion 优先于 status）。 */
export function runStatusIconKind(
  status: string | undefined | null,
  conclusion: string | null | undefined,
): GhStatusIconKind {
  // 已完成 → 按 conclusion 决定
  if (conclusion === 'success') return 'success';
  if (conclusion === 'failure' || conclusion === 'timed_out') return 'failure';
  if (conclusion === 'cancelled' || conclusion === 'skipped') return 'cancelled';
  if (conclusion === 'action_required') return 'action_required';
  if (conclusion) return 'unknown';
  // 未完成 → 按 status
  if (status && ['queued', 'in_progress', 'waiting', 'requested', 'pending'].includes(status)) {
    return 'active';
  }
  return 'unknown';
}

/** Git 推送事件 → 图标名的最小映射（供 RunRow 元信息行）。 */
export function ghEventIconKind(event: string | undefined | null): string | null {
  if (!event) return null;
  return event;
}
