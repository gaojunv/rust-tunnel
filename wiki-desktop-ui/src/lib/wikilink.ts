// wikilink → markdown 链接的预转换。
// 规则：行级扫描 + 小状态机；跳过围栏代码块（``` / ~~~，至多 3 个前置空格，记住字符+长度）
// 和行内反引号区间；其余文本将 [[target]] / [[target|label]] 转为 [label](#/note/<enc>)。

export const WIKILINK_PREFIX = "#/note/";

export function parseWikilinkHref(href: string): string | null {
  if (!href.startsWith(WIKILINK_PREFIX)) return null;
  const enc = href.slice(WIKILINK_PREFIX.length);
  try {
    return decodeURIComponent(enc);
  } catch {
    return enc;
  }
}

function fenceInfo(line: string): { ch: string; len: number } | null {
  let i = 0;
  let spaces = 0;
  while (i < line.length && line[i] === " " && spaces < 3) {
    i++;
    spaces++;
  }
  if (i < line.length && line[i] === " ") return null; // 第 4 个空格 → 缩进代码，非围栏
  const ch = line[i];
  if (ch !== "`" && ch !== "~") return null;
  let len = 0;
  while (i + len < line.length && line[i + len] === ch) len++;
  if (len < 3) return null;
  return { ch, len };
}

// 将单行中非代码区间的 wikilink 替换为 markdown 链接
function replaceWikilinksInLine(line: string): string {
  // 先找出行内反引号区间（成对反引号之内的内容为代码，不转换）
  // 规则：连续 1 个或多个反引号作为定界符，匹配最短闭合
  const codeSpans: Array<[number, number]> = [];
  {
    let i = 0;
    while (i < line.length) {
      if (line[i] !== "`") {
        i++;
        continue;
      }
      let openLen = 0;
      while (i + openLen < line.length && line[i + openLen] === "`") openLen++;
      const openEnd = i + openLen;
      const closeIdx = line.indexOf("`".repeat(openLen), openEnd);
      if (closeIdx === -1) break; // 未闭合，剩余都算代码（CommonMark 语义下未闭合行内码延伸到行尾）
      codeSpans.push([i, closeIdx + openLen]);
      i = closeIdx + openLen;
    }
  }

  function inCode(pos: number): boolean {
    for (const [s, e] of codeSpans) if (pos >= s && pos < e) return true;
    return false;
  }

  // 逐字符扫描 [[ ... ]]，遇到位于 code span 内的则跳过
  let out = "";
  let i = 0;
  while (i < line.length) {
    if (inCode(i)) {
      // 跳到该代码区间末尾
      for (const [s, e] of codeSpans) {
        if (i >= s && i < e) {
          out += line.slice(i, e);
          i = e;
          break;
        }
      }
      continue;
    }
    if (line[i] === "[" && line[i + 1] === "[") {
      const close = line.indexOf("]]", i + 2);
      if (close === -1) {
        out += line[i];
        i++;
        continue;
      }
      const inner = line.slice(i + 2, close);
      if (!inner.trim()) {
        // 空内容，保持原样
        out += line.slice(i, close + 2);
        i = close + 2;
        continue;
      }
      const bar = inner.indexOf("|");
      const target = (bar === -1 ? inner : inner.slice(0, bar)).trim();
      const label = (bar === -1 ? inner : inner.slice(bar + 1)).trim() || target;
      if (!target) {
        out += line.slice(i, close + 2);
        i = close + 2;
        continue;
      }
      const href = `${WIKILINK_PREFIX}${encodeURIComponent(target)}`;
      // 对 label 中的 ] 做最小转义，避免破坏 markdown 链接结构
      const safeLabel = label.replace(/\]/g, "\\]");
      out += `[${safeLabel}](${href})`;
      i = close + 2;
      continue;
    }
    out += line[i];
    i++;
  }
  return out;
}

export function transformWikilinks(md: string): string {
  const lines = md.split("\n");
  let inFence: { ch: string; len: number } | null = null;

  const outLines: string[] = [];
  for (const line of lines) {
    const fi = fenceInfo(line);
    if (inFence) {
      outLines.push(line);
      // 闭合条件：同字符、长度 >= 开启长度，其余字符仅允许空白（CommonMark 宽松实现：有内容也算闭合前的内容行）
      // 简化：只要出现同字符且长度 >= 开启长度的 fence 行即闭合
      if (fi && fi.ch === inFence.ch && fi.len >= inFence.len) {
        inFence = null;
      }
      continue;
    }
    if (fi) {
      inFence = fi;
      outLines.push(line);
      continue;
    }
    outLines.push(replaceWikilinksInLine(line));
  }
  return outLines.join("\n");
}
