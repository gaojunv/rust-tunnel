/** 从 extra_config JSON 读 agent_context_limit；非法/缺省返回 ''。 */
export function parseContextLimit(extraConfig?: string | null): string {
  if (!extraConfig) return '';
  try {
    const v = (JSON.parse(extraConfig) as { agent_context_limit?: unknown }).agent_context_limit;
    return typeof v === 'number' ? String(v) : '';
  } catch {
    return '';
  }
}

/** 把 agent_context_limit 合并回 extra_config JSON；空值删除该键，保留其他键。 */
export function mergeContextLimit(extraConfig: string | null | undefined, limit: string): string | null {
  let obj: Record<string, unknown> = {};
  if (extraConfig) { try { obj = JSON.parse(extraConfig) as Record<string, unknown>; } catch { obj = {}; } }
  const n = Number(limit);
  if (limit.trim() && Number.isFinite(n) && n > 0) obj.agent_context_limit = Math.floor(n);
  else delete obj.agent_context_limit;
  return Object.keys(obj).length ? JSON.stringify(obj) : null;
}
