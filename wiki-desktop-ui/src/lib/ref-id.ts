/**
 * 服务端 ref 规范镜像（与 Rust `normalize_wiki_ref` 严格一致）
 * 规则：trim+lowercase 后非空、≤128、仅 [a-z0-9/_-]、首字符 [a-z0-9]、不含 // ./ ../
 */

/**
 * 归一化远程 ref，非法返回 null
 * 与 Rust `normalize_wiki_ref` 判定顺序保持一致
 */
export function normalizeRemoteRef(raw: string): string | null {
  const s = raw.trim().toLowerCase();
  if (s.length === 0 || s.length > 128) return null;
  if (s.includes("//") || s.includes("./") || s.includes("../")) return null;
  const first = s.charCodeAt(0);
  const isLower = first >= 97 && first <= 122; // a-z
  const isDigit = first >= 48 && first <= 57; // 0-9
  if (!isLower && !isDigit) return null;
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    const isAz = c >= 97 && c <= 122;
    const is09 = c >= 48 && c <= 57;
    const isSlash = c === 47; // /
    const isUnder = c === 95; // _
    const isDash = c === 45; // -
    if (!isAz && !is09 && !isSlash && !isUnder && !isDash) return null;
  }
  return s;
}

/**
 * 将本地 key 映射为远程 ref
 * 优先 frontmatterRef（需通过 normalizeRemoteRef），否则 key 直接过校验
 * key 若含大写则视为不兼容（需 frontmatter 显式指定），以避免大小写自动归一导致的意外覆盖
 * @param key 本地笔记 key
 * @param frontmatterRef frontmatter 中显式声明的 ref，可为 null/undefined
 */
export function toRemoteRef(key: string, frontmatterRef?: string | null): string | null {
  // 前置 frontmatter 优先
  if (frontmatterRef != null && frontmatterRef.trim() !== "") {
    const n = normalizeRemoteRef(frontmatterRef);
    // 前置存在时以其为准，非法直接返回 null（不回退到 key）
    return n;
  }
  // 无 frontmatter 时，key 必须已是小写合法形态
  // 若 key 含大写字母，则视为不兼容（需显式 frontmatter）
  const trimmed = key.trim();
  if (trimmed !== trimmed.toLowerCase()) return null;
  return normalizeRemoteRef(key);
}
