// 每篇笔记编辑区滚动位置记忆（模块级内存，不持久化）
// key = noteKey, value = 编辑区 scrollTop
const store = new Map<string, number>();

/** 读取某篇笔记的编辑区滚动位置；无记录返回 0 */
export function readScrollPos(key: string): number {
  if (!key) return 0;
  return store.get(key) ?? 0;
}

/** 写入某篇笔记的编辑区滚动位置；key 空串忽略 */
export function writeScrollPos(key: string, pos: number): void {
  if (!key) return;
  store.set(key, pos);
}

/** 仅供测试：清空内存 */
export function __clearScrollMemory(): void {
  store.clear();
}
