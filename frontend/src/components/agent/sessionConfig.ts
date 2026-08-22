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

/** 取选项当前值（归一化：boolean → 布尔真值 currentBool，select → currentValue 字符串）。
 * 与 [`restoreConfigValue`] 配套，用于乐观更新回滚快照（M19）的存取。 */
export function optionValue(o: SessionConfigOption): string | boolean {
  return o.type === 'boolean' ? (o.currentBool ?? false) : (o.currentValue ?? '');
}

/** 从持久化 config_state（JSON map：config_id → value）提取 model 配置项的值
 * （claude-code 为 opus/sonnet/haiku tier）。供 SessionSettingsMenu 在 WS 快照
 * 未达（configOptions 为空/未含 model 项）时作显示种子——切页/刷新后 ACP 进程
 * 已被回收的场景下，模型选择不回退显示默认。解析失败/非对象/无 model 键返回 undefined。 */
export function configStateModelValue(configState: string | null | undefined): string | undefined {
  if (!configState || !configState.trim()) return undefined;
  try {
    const parsed: unknown = JSON.parse(configState);
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return undefined;
    const v = (parsed as Record<string, unknown>).model;
    return typeof v === 'string' && v.trim() ? v.trim() : undefined;
  } catch {
    return undefined;
  }
}

/** 把选项恢复到某前值：boolean 同时写回 currentBool 与 "true"/"false" 的
 * currentValue（保持归一化形态），select 写 currentValue。 */
export function restoreConfigValue(
  o: SessionConfigOption,
  prev: string | boolean,
): SessionConfigOption {
  if (o.type === 'boolean') {
    const b = Boolean(prev);
    return { ...o, currentBool: b, currentValue: b ? 'true' : 'false' };
  }
  return { ...o, currentValue: String(prev) };
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
