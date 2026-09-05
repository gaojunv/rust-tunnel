/**
 * 纯逻辑：findLinkQuery + buildInsertion
 * 与组件解耦，便于单测
 */

/**
 * 从 caret 向前扫同一行找最近的未闭合 [[
 * - 同一行内（遇换行即止）
 * - 中间无 ]] 且无换行
 * - query 含 | 或 # 时返回 null（用户已在写别名/锚点，不补全）
 * - 简易代码块判定：该行以 ``` 或 4 空格开头则不激活
 */
export function findLinkQuery(
  text: string,
  caret: number,
): { start: number; query: string } | null {
  const clamped = Math.max(0, Math.min(caret, text.length));
  // 找到 caret 所在行的起点
  const lineStart = text.lastIndexOf("\n", clamped - 1) + 1;
  const line = text.slice(lineStart, clamped);
  // 简易代码块判定：该行以 ``` 或 4 空格开头则跳过（近似取舍：不做完整围栏解析）
  const nlIdx = text.indexOf("\n", lineStart);
  const fullLine = text.slice(lineStart, nlIdx === -1 ? text.length : nlIdx);
  if (fullLine.startsWith("```") || fullLine.startsWith("    ")) {
    return null;
  }

  const openIdx = line.lastIndexOf("[[");
  if (openIdx === -1) return null;
  const afterOpen = line.slice(openIdx + 2);
  // 中间若已有 ]] 视为已闭合
  if (afterOpen.includes("]]")) return null;
  const query = afterOpen;
  if (query.includes("|") || query.includes("#")) return null;
  const start = lineStart + openIdx;
  return { start, query };
}

/** 选中行为：query 为空或等于 key 的 basename（大小写不敏感）则 [[key]]，否则 [[key|query]] */
export function buildInsertion(key: string, query: string): string {
  if (!query) return `[[${key}]]`;
  const basename = key.split("/").pop() ?? key;
  if (query.toLowerCase() === basename.toLowerCase()) return `[[${key}]]`;
  return `[[${key}|${query}]]`;
}
