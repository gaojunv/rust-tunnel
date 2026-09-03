/**
 * 大小写不敏感的子序列模糊匹配。
 * CJK 按字符逐字匹配，无拼音。
 */

/** 是否为词边界字符（用于加分） */
function isBoundary(ch: string): boolean {
  return ch === " " || ch === "/" || ch === "-" || ch === "_" || ch === ".";
}

/**
 * 返回 query 在 candidate 中按顺序匹配的字符索引。
 * 找不到则返回空数组。
 */
export function matchIndices(candidate: string, query: string): number[] {
  if (!query) return [];
  const cLower = candidate.toLowerCase();
  const qLower = query.toLowerCase();
  const out: number[] = [];
  let ci = 0;
  for (let qi = 0; qi < qLower.length; qi++) {
    const qch = qLower[qi];
    let found = -1;
    for (let j = ci; j < cLower.length; j++) {
      if (cLower[j] === qch) {
        found = j;
        break;
      }
    }
    if (found === -1) return [];
    out.push(found);
    ci = found + 1;
  }
  return out;
}

/**
 * 模糊评分：子序列匹配 + 连续/边界加分 - 长度惩罚。
 * 匹配失败返回 null，未匹配时调用方可过滤。
 */
export function fuzzyScore(candidate: string, query: string): number | null {
  if (!query) return 0;
  const indices = matchIndices(candidate, query);
  if (indices.length !== query.length) return null;
  if (indices.length === 0) return null;

  let score = 0;
  for (let i = 0; i < indices.length; i++) {
    const pos = indices[i];
    score += 1; // 基础分
    if (i > 0 && pos === indices[i - 1] + 1) score += 2; // 连续加分
    if (pos === 0 || isBoundary(candidate[pos - 1])) score += 1.5; // 词首/边界加分
  }
  score -= candidate.length * 0.01; // 长度惩罚
  return score;
}
