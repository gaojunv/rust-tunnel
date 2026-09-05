/**
 * 服务端 JWT 存储 —— 内存 + localStorage 双写，按 baseUrl 隔离。
 * localStorage 读写全部 try/catch，兼容隐私模式 / 禁用存储的异常。
 */

/** 401 触发的认证过期错误 */
export class AuthExpiredError extends Error {
  constructor(message = "认证已过期") {
    super(message);
    this.name = "AuthExpiredError";
  }
}

/** 内存缓存，按 baseUrl 归一化后的 key 索引 */
const mem = new Map<string, string>();

/**
 * 归一化 baseUrl：trim 并去掉尾随 `/`
 */
function normalizeBaseUrl(baseUrl: string): string {
  return baseUrl.trim().replace(/\/+$/, "");
}

/**
 * 简单字符串 hash（djb2 变体），用于生成 per-baseUrl 的存储 key。
 * 输出为无符号 32 位整数的 16 进制字符串。
 */
function hashString(s: string): string {
  let h = 5381;
  for (let i = 0; i < s.length; i++) {
    h = ((h << 5) + h + s.charCodeAt(i)) | 0;
  }
  return (h >>> 0).toString(16);
}

/** 计算隔离存储 key */
function tokenKey(baseUrl: string): string {
  const norm = normalizeBaseUrl(baseUrl);
  return `wiki.auth.${hashString(norm)}`;
}

/**
 * 获取指定服务端的 JWT
 * 优先内存，其次 localStorage
 */
export function getToken(baseUrl: string): string | null {
  const key = tokenKey(baseUrl);
  if (mem.has(key)) return mem.get(key)!;
  try {
    const v = localStorage.getItem(key);
    if (v) mem.set(key, v);
    return v;
  } catch {
    return null;
  }
}

/**
 * 保存指定服务端的 JWT（内存 + localStorage 双写）
 */
export function setToken(baseUrl: string, token: string): void {
  const key = tokenKey(baseUrl);
  mem.set(key, token);
  try {
    localStorage.setItem(key, token);
  } catch {
    // 隐私模式或存储配额异常忽略
  }
}

/**
 * 清除指定服务端的 JWT
 */
export function clearToken(baseUrl: string): void {
  const key = tokenKey(baseUrl);
  mem.delete(key);
  try {
    localStorage.removeItem(key);
  } catch {
    // 忽略
  }
}

/**
 * 清除所有服务端的 JWT（内存全清 + localStorage 前缀扫描）
 */
export function clearAllTokens(): void {
  mem.clear();
  try {
    const toRemove: string[] = [];
    for (let i = 0; i < localStorage.length; i++) {
      const k = localStorage.key(i);
      if (k && k.startsWith("wiki.auth.")) toRemove.push(k);
    }
    for (const k of toRemove) localStorage.removeItem(k);
  } catch {
    // 忽略
  }
}

/** 仅测试用：重置内存（不碰 localStorage） */
export function _resetMemForTest(): void {
  mem.clear();
}
