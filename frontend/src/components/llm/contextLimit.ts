/** 两档上下文上限常量（内部单位为 chars，chars/4 ≈ tokens）。 */
export const CONTEXT_LIMIT_OPTIONS = [
  { value: '256k' as const, chars: 1_048_576 }, // 256K tokens × 4
  { value: '1m' as const, chars: 4_194_304 },   // 1M tokens × 4
];

export type ContextLimitTier = '256k' | '1m';

/** 从 extra_config JSON 读 agent_context_limit（chars）并映射到档位；非法/缺省返回 '256k'。 */
export function parseContextLimit(extraConfig?: string | null): ContextLimitTier {
  if (!extraConfig) return '256k';
  try {
    const v = (JSON.parse(extraConfig) as { agent_context_limit?: unknown }).agent_context_limit;
    if (typeof v === 'number' && v >= 4_194_304) return '1m';
    return '256k';
  } catch {
    return '256k';
  }
}

/** 把档位合并回 extra_config JSON；'256k' 删除该键（等于默认值，保持配置干净），保留其他键。 */
export function mergeContextLimit(extraConfig: string | null | undefined, tier: ContextLimitTier): string | null {
  let obj: Record<string, unknown> = {};
  if (extraConfig) { try { obj = JSON.parse(extraConfig) as Record<string, unknown>; } catch { obj = {}; } }
  if (tier === '1m') {
    obj.agent_context_limit = 4_194_304;
  } else {
    delete obj.agent_context_limit;
  }
  return Object.keys(obj).length ? JSON.stringify(obj) : null;
}
