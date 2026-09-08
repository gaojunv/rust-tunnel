/**
 * 服务端 ref 规范镜像（与 Rust `normalize_wiki_ref` 严格一致）
 * 规则：trim+lowercase 后非空、≤128、仅 [a-z0-9/_-]、首字符 [a-z0-9]、不含 // ./ ../
 * 不兼容 key 的派生规则：`n-` + SHA-256(UTF-8(key)) hex 的前 12 个字符
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

/**
 * 校验/归一化服务端 `origin_key` 为安全的本地 key：
 * - trim；空 → null；[...s].length > 200 → null
 * - `\` → `/`
 * - 拒绝：以 `/` 开头；`^[a-zA-Z]:` 盘符；含 `<>:"|?*` 或控制字符（charCode < 32）
 * - 拒绝：任何 `/` 段为 `/`/`.`/`..`；段末为空格或 `.`（Windows 兼容）
 * - 拒绝：最后一段（去扩展名、大小写不敏感）是 Windows 保留名
 * 通过则返回归一化后的串
 */
export function sanitizeOriginKey(raw: string): string | null {
  let s = raw.trim();
  if (s.length === 0) return null;
  // `\` → `/`
  s = s.replace(/\\/g, "/");
  // 字符数检查
  if ([...s].length > 200) return null;
  // 不允许绝对路径 / 盘符
  if (s.startsWith("/")) return null;
  if (/^[a-zA-Z]:/.test(s)) return null;
  // 拒绝非法字符 + 控制字符
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c < 32) return null;
    const ch = s[i];
    if ("<>:\"|?*".includes(ch)) return null;
  }
  // 段检查
  const segments = s.split("/");
  for (let i = 0; i < segments.length; i++) {
    const seg = segments[i];
    if (seg === "" || seg === "." || seg === "..") return null;
    if (seg.endsWith(" ") || seg.endsWith(".")) return null;
  }
  // Windows 保留名：最后一段（去扩展名，大小写不敏感）
  const lastSeg = segments[segments.length - 1] ?? "";
  const dotIdx = lastSeg.lastIndexOf(".");
  const baseName = (dotIdx > 0 ? lastSeg.slice(0, dotIdx) : lastSeg).toUpperCase();
  const RESERVED = new Set([
    "CON", "PRN", "AUX", "NUL",
    ...Array.from({ length: 9 }, (_, i) => `COM${i + 1}`),
    ...Array.from({ length: 9 }, (_, i) => `LPT${i + 1}`),
  ]);
  if (RESERVED.has(baseName)) return null;
  return s;
}

/**
 * 派生确定性 ref：`n-` + SHA-256(UTF-8(key)) hex 的前 12 个字符
 * 与 sync-engine.ts 中 hashNote 的 crypto.subtle 用法一致
 */
export async function deriveRefFromKey(key: string): Promise<string> {
  const data = new TextEncoder().encode(key);
  const buf = await crypto.subtle.digest("SHA-256", data);
  const arr = new Uint8Array(buf);
  const hex = Array.from(arr)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
  return `n-${hex.slice(0, 12)}`;
}
