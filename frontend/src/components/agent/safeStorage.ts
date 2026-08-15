/**
 * localStorage 安全访问包装：隐私模式/禁用 storage 时 getItem/setItem/removeItem
 * 会抛异常（SecurityError 等），直接访问会让整个组件崩溃。agent 工作台的
 * localStorage 读写统一走此包装（读失败 → null，写失败 → 静默丢弃，语义与
 * tabsStore 既有「本地记忆仅是增强」一致）。
 */

/** 读取 localStorage；不可用/异常 → null（调用方按「无持久化」处理）。 */
export function safeLocalStorageGet(key: string): string | null {
  try {
    return typeof window !== 'undefined' ? window.localStorage.getItem(key) : null;
  } catch {
    return null;
  }
}

/** 写入 localStorage；不可用/容量满等异常静默丢弃（本地记忆仅是增强）。 */
export function safeLocalStorageSet(key: string, value: string): void {
  try {
    if (typeof window !== 'undefined') window.localStorage.setItem(key, value);
  } catch {
    /* 存储不可用静默失败 */
  }
}

/** 删除 localStorage 键；不可用/异常静默忽略。 */
export function safeLocalStorageRemove(key: string): void {
  try {
    if (typeof window !== 'undefined') window.localStorage.removeItem(key);
  } catch {
    /* 存储不可用静默失败 */
  }
}
