// 每篇笔记编辑区滚动位置记忆（localStorage 持久化 + 内存缓存）
// key = noteKey, value = 编辑区 scrollTop
// 持久化：`wiki.scroll-pos.v1` → `{ [noteKey]: scrollTop }`，上限 200 条（LRU 淘汰）
// localStorage 不可用/抛错（隐私模式、无痕配额、SSR、单测 node 环境）时退化为纯内存行为
// 懒加载：首次读写时才从 localStorage 恢复
// 对外 API（readScrollPos / writeScrollPos / __clearScrollMemory）签名不变

/** localStorage 持久化 key */
export const SCROLL_MEMORY_STORAGE_KEY = "wiki.scroll-pos.v1";

/** 持久化容量上限；超出时淘汰最久未使用的条目 */
export const SCROLL_MEMORY_MAX_ENTRIES = 200;

const store = new Map<string, number>();
let loaded = false;

function storage(): Storage | null {
  try {
    if (typeof window === "undefined" || window.localStorage == null) return null;
    return window.localStorage;
  } catch {
    // 极少数沙箱环境访问 window.localStorage 本身抛错
    return null;
  }
}

/** 首次访问时从 localStorage 恢复；坏数据/被拦截则保持空内存 */
function ensureLoaded(): void {
  if (loaded) return;
  loaded = true;
  const s = storage();
  if (!s) return;
  try {
    const raw = s.getItem(SCROLL_MEMORY_STORAGE_KEY);
    if (raw == null) return;
    const parsed: unknown = JSON.parse(raw);
    if (parsed == null || typeof parsed !== "object" || Array.isArray(parsed)) return;
    const entries: Array<[string, number]> = [];
    for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
      if (typeof v === "number" && Number.isFinite(v)) entries.push([k, v]);
    }
    // 序列化顺序即 LRU 顺序（越新越靠后）：只保留最新的 MAX 条
    for (const [k, v] of entries.slice(-SCROLL_MEMORY_MAX_ENTRIES)) {
      store.set(k, v);
    }
  } catch {
    // 坏 JSON / 读取被拦截：退化为纯内存行为
  }
}

/** 全量写回 localStorage；失败静默忽略（配额/隐私模式） */
function persist(): void {
  const s = storage();
  if (!s) return;
  try {
    const obj: Record<string, number> = {};
    for (const [k, v] of store) obj[k] = v;
    s.setItem(SCROLL_MEMORY_STORAGE_KEY, JSON.stringify(obj));
  } catch {
    // 存储失败静默忽略，保持内存行为
  }
}

/** 读取某篇笔记的编辑区滚动位置；无记录返回 0 */
export function readScrollPos(key: string): number {
  if (!key) return 0;
  ensureLoaded();
  const v = store.get(key);
  if (v === undefined) return 0;
  // LRU：命中即视为最新，移到末尾并写回
  store.delete(key);
  store.set(key, v);
  persist();
  return v;
}

/** 写入某篇笔记的编辑区滚动位置；key 空串或非有限数值忽略 */
export function writeScrollPos(key: string, pos: number): void {
  if (!key) return;
  if (!Number.isFinite(pos)) return;
  ensureLoaded();
  // 已存在先删，保证 Map 尾部 = 最新
  store.delete(key);
  store.set(key, pos);
  // 超限时从头部（最久未用）淘汰；Map 删除迭代安全
  for (const oldest of store.keys()) {
    if (store.size <= SCROLL_MEMORY_MAX_ENTRIES) break;
    store.delete(oldest);
  }
  persist();
}

/** 仅供测试：清空内存与持久化 */
export function __clearScrollMemory(): void {
  store.clear();
  // 标记已加载：即使 removeItem 被拦截丢数据，也保证"清空"视图一致
  loaded = true;
  const s = storage();
  if (!s) return;
  try {
    s.removeItem(SCROLL_MEMORY_STORAGE_KEY);
  } catch {
    // 忽略，见上
  }
}
