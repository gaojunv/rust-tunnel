import type { SessionConfigOption, SessionConfigSelectOption } from '../../types';

/** ACP config_options 原始 JSON → 前端归一化 SessionConfigOption[]。
 *  - grouped options 拍平为 ungrouped；
 *  - boolean 的 currentValue(bool) 填入 currentBool，currentValue 置 "true"/"false"；
 *  - 无法识别的项原样保留（category 缺省/未知不丢弃）。 */
export function normalizeConfigOptions(raw: unknown): SessionConfigOption[] {
  if (!Array.isArray(raw)) return [];
  const out: SessionConfigOption[] = [];
  for (const item of raw) {
    if (!item || typeof item !== 'object') continue;
    const o = item as Record<string, unknown>;
    if (typeof o.id !== 'string' || typeof o.type !== 'string') continue;
    const base: SessionConfigOption = {
      id: o.id,
      name: typeof o.name === 'string' ? o.name : o.id,
      description: typeof o.description === 'string' ? o.description : undefined,
      category: typeof o.category === 'string' ? o.category : undefined,
      type: o.type === 'boolean' ? 'boolean' : 'select',
    };
    if (base.type === 'boolean') {
      const b = o.currentValue === true;
      base.currentBool = b;
      base.currentValue = b ? 'true' : 'false';
    } else {
      base.currentValue = typeof o.currentValue === 'string' ? o.currentValue : '';
      base.options = flattenOptions(o.options);
    }
    out.push(base);
  }
  return out;
}

/** option 当前取值的展示名（select：name 表查找，找不到回退 value-id 原文）。 */
export function currentOptionLabel(o: SessionConfigOption): string {
  return o.options?.find((v) => v.value === o.currentValue)?.name ?? String(o.currentValue ?? '');
}

function flattenOptions(raw: unknown): SessionConfigSelectOption[] {
  if (!Array.isArray(raw)) return [];
  const out: SessionConfigSelectOption[] = [];
  for (const entry of raw) {
    if (!entry || typeof entry !== 'object') continue;
    const e = entry as Record<string, unknown>;
    if (typeof e.value === 'string') {
      out.push({
        value: e.value,
        name: typeof e.name === 'string' ? e.name : e.value,
        description: typeof e.description === 'string' ? e.description : undefined,
      });
    } else if (Array.isArray(e.options)) {
      out.push(...flattenOptions(e.options));
    }
  }
  return out;
}
