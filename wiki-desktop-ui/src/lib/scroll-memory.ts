// 每篇笔记滚动位置记忆（模块级内存，不持久化）
// key = noteKey, value = { edit, preview }
const store = new Map<string, { edit: number; preview: number }>();

/** 读取某篇笔记的滚动位置；无记录返回 {0,0} */
export function readScrollPos(key: string): { edit: number; preview: number } {
  if (!key) return { edit: 0, preview: 0 };
  const v = store.get(key);
  if (!v) return { edit: 0, preview: 0 };
  return { edit: v.edit, preview: v.preview };
}

/** 写入某篇笔记单侧滚动位置；key 空串忽略 */
export function writeScrollPos(key: string, mode: "edit" | "preview", pos: number): void {
  if (!key) return;
  const cur = store.get(key) ?? { edit: 0, preview: 0 };
  if (mode === "edit") {
    store.set(key, { edit: pos, preview: cur.preview });
  } else {
    store.set(key, { edit: cur.edit, preview: pos });
  }
}

/** 仅供测试：清空内存 */
export function __clearScrollMemory(): void {
  store.clear();
}
