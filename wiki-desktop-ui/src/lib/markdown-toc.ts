/**
 * TOC 抽取 —— 按行扫描，fence 状态机跳过 ```/~~~ 代码块
 */
export interface TocItem {
  level: number;
  text: string;
  line: number; // 从 0 计
}

function isFenceLine(line: string): { ch: string; len: number } | null {
  let i = 0;
  let spaces = 0;
  while (i < line.length && line[i] === " " && spaces < 3) {
    i++;
    spaces++;
  }
  if (i < line.length && line[i] === " ") return null;
  const ch = line[i];
  if (ch !== "`" && ch !== "~") return null;
  let len = 0;
  while (i + len < line.length && line[i + len] === ch) len++;
  if (len < 3) return null;
  return { ch, len };
}

export function extractToc(body: string): TocItem[] {
  if (!body) return [];
  const lines = body.split("\n");
  const out: TocItem[] = [];
  let inFence: { ch: string; len: number } | null = null;
  const headingRe = /^(#{1,6})\s+(.+?)\s*#*$/;

  for (let idx = 0; idx < lines.length; idx++) {
    const line = lines[idx];
    const fi = isFenceLine(line);
    if (inFence) {
      if (fi && fi.ch === inFence.ch && fi.len >= inFence.len) {
        inFence = null;
      }
      continue;
    }
    if (fi) {
      inFence = fi;
      continue;
    }
    const m = headingRe.exec(line);
    if (m) {
      const level = m[1].length;
      const text = m[2].trim();
      if (text) out.push({ level, text, line: idx });
    }
  }
  return out;
}

/**
 * 当前标题下标 —— items 中最后一个 line <= cursorLine 的标题；
 * cursorLine 为 null/无标题/光标在首个标题之前时返回 -1。
 */
export function resolveActiveTocIndex(items: TocItem[], cursorLine: number | null): number {
  if (cursorLine == null || items.length === 0) return -1;
  let active = -1;
  for (let i = 0; i < items.length; i++) {
    if (items[i].line <= cursorLine) active = i;
    else break;
  }
  return active;
}
