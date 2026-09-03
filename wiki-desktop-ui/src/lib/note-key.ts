/**
 * 与后端 `vault_ops::resolve_note_path` 镜像的前端 key 校验。
 * 规则（中文错误文案）：
 *  - trim 后非空
 *  - 不含 `:` 或 `\`
 *  - 非前导 `/`
 *  - 非尾随 `/`
 *  - 无 `..` 段（按 `/` 分割）
 */
export function normalizeNoteKey(raw: string): string {
  const trimmed = raw.trim();
  // 去掉尾随的所有 `/`
  if (trimmed.endsWith("/")) return trimmed.replace(/\/+$/, "");
  return trimmed;
}

export function validateNoteKey(raw: string): string | null {
  const trimmed = raw.trim();
  if (!trimmed) return "标题不能为空";
  if (trimmed.includes(":") || trimmed.includes("\\")) return "标题不能包含 : 或 \\";
  if (trimmed.startsWith("/")) return "标题不能以 / 开头";
  if (trimmed.endsWith("/")) return "标题不能以 / 结尾";
  const segs = trimmed.split("/");
  if (segs.some((s) => s === "..")) return "标题不能包含 .. 路径段";
  return null;
}
