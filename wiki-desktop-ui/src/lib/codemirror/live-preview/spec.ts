import type { EditorState } from "@codemirror/state";
import { syntaxTree } from "@codemirror/language";
import type { SyntaxNode, SyntaxNodeRef } from "@lezer/common";
import { scanWikilinksInLine } from "@/lib/wikilink";
import { isAttachmentSrc, normalizeAttachmentSrc } from "@/lib/attachments";

/**
 * Live Preview 纯函数核心。
 *
 * 只依赖 `@codemirror/state` / `@codemirror/language` / lezer（经由
 * `syntaxTree` 与 `SyntaxNode` API），**不得 import `@codemirror/view`**，
 * 以便在 vitest node 环境下直接测试。
 *
 * 与 `wikiSyntaxHighlighting` 的分层约定：
 * - highlight 层管内容级样式（h1-h3 字号、粗斜体、行内码配色）；
 * - 本层只管结构级装饰：隐藏标记符、widget 替换位、行背景、h4-h6 字号补齐。
 *   因此行内构造（Strong/Emphasis/删除线/行内码）只发射 `hide` spec，
 *   不再叠加内容样式，避免 em 尺寸叠乘与行高抖动。
 */

/** 结构级装饰类名（`cm-lp-` 前缀，theme.ts 消费同一份常量）。 */
export const LP_CLASS = {
  h1: "cm-lp-h1",
  h2: "cm-lp-h2",
  h3: "cm-lp-h3",
  h4: "cm-lp-h4",
  h5: "cm-lp-h5",
  h6: "cm-lp-h6",
  quote: "cm-lp-quote",
  listMark: "cm-lp-listmark",
  codeLine: "cm-lp-codeblock-line",
} as const;

/**
 * 可序列化的装饰描述（视图无关，`index.ts` 负责映射为 Decoration）。
 * - `line`: 整行 class（标题级别、引用、围栏代码行背景）
 * - `mark`: 行内区间 class（ListMark 弱化）
 * - `hide`: 隐藏区间（标记符等，视图层映射为 `Decoration.replace({})`）
 * - `checkbox`: TaskMarker 替换为复选框 widget
 * - `hr`: 分隔线替换为 widget
 * - `wikilink`: `[[target|label]]` 替换为链接 widget
 * - `image`: 图片替换为渲染 widget（block/inline）
 * - `mdlink`: Markdown 链接 `[text](url)` 替换为渲染 widget
 * - `table`: GFM 表格替换为 HTML table widget
 * - `codeheader`: 代码块开围栏行（含语言标签）替换为细条 widget
 * - `codefooter`: 代码块闭围栏行替换为零高度 widget
 * - `frontmatter`: YAML frontmatter 区块折叠为单行 muted 细条 widget
 */
export type DecoSpec =
  | { kind: "line"; line: number; cls: string }
  | { kind: "mark"; from: number; to: number; cls: string }
  | { kind: "hide"; from: number; to: number }
  | { kind: "checkbox"; from: number; to: number; checked: boolean }
  | { kind: "hr"; from: number; to: number }
  | { kind: "wikilink"; from: number; to: number; target: string; label: string }
  | { kind: "image"; from: number; to: number; alt: string; src: string; block: boolean }
  | { kind: "mdlink"; from: number; to: number; text: string; url: string }
  | { kind: "table"; from: number; to: number; raw: string }
  | { kind: "codeheader"; from: number; to: number; lang: string }
  | { kind: "codefooter"; from: number; to: number }
  | { kind: "frontmatter"; from: number; to: number; propCount: number }
  | { kind: "callout"; from: number; to: number; calloutType: string; title: string }
  | { kind: "highlight"; from: number; to: number; markFrom: number; markTo: number };

type SelRange = { from: number; to: number };

/** 块级还原：任一选区与 [lineFrom, lineTo] 相交即还原该行。 */
function touchesLine(ranges: readonly SelRange[], lineFrom: number, lineTo: number): boolean {
  for (const r of ranges) {
    if (r.from <= lineTo && r.to >= lineFrom) return true;
  }
  return false;
}

/**
 * 行内还原：任一选区与 [from - pad, to + pad] 相交即还原。
 * pad 取标记符长度——光标紧贴标记符边界时不闪烁。
 */
function touchesSpan(
  ranges: readonly SelRange[],
  from: number,
  to: number,
  pad: number,
): boolean {
  const lo = from - pad;
  const hi = to + pad;
  for (const r of ranges) {
    if (r.from <= hi && r.to >= lo) return true;
  }
  return false;
}

const ATX_RE = /^ATXHeading([1-6])$/;
const SETEXT_RE = /^SetextHeading([12])$/;

/** callout 首行匹配：引用块去 `>` 前缀后，检测 `[!type]` + 可选展开/折叠标记 + 标题 */
const CALLOUT_RE = /^\[!([^\]]+)\]\s*[+-]?\s*(.*)/;

/** 仅允许 [a-z0-9-] 的类型名（小写化后过滤，防注入） */
function sanitizeCalloutType(raw: string): string {
  return raw.toLowerCase().replace(/[^a-z0-9-]/g, "");
}

/**
 * 扫描单行中非行内代码区间的 `==highlight==` 配对。
 * 返回各匹配的 (innerFrom, innerTo) —— 即 `==` 标记之间的纯内容区间。
 */
function scanHighlightsInLine(
  line: string,
  baseOffset: number,
): Array<{ innerFrom: number; innerTo: number; markFrom: number; markTo: number }> {
  const results: Array<{ innerFrom: number; innerTo: number; markFrom: number; markTo: number }> = [];
  // 收集行内代码区间（同 wikilink.ts collectCodeSpans 逻辑）
  const codeSpans: Array<[number, number]> = [];
  let ci = 0;
  while (ci < line.length) {
    if (line[ci] !== "`") { ci++; continue; }
    let openLen = 0;
    while (ci + openLen < line.length && line[ci + openLen] === "`") openLen++;
    const openEnd = ci + openLen;
    const closeIdx = line.indexOf("`".repeat(openLen), openEnd);
    if (closeIdx === -1) break;
    codeSpans.push([ci, closeIdx + openLen]);
    ci = closeIdx + openLen;
  }
  function inCode(pos: number): boolean {
    for (const [s, e] of codeSpans) if (pos >= s && pos < e) return true;
    return false;
  }
  let i = 0;
  while (i < line.length - 3) {
    if (inCode(i)) {
      for (const [s, e] of codeSpans) { if (i >= s && i < e) { i = e; break; } }
      continue;
    }
    if (line[i] === "=" && line[i + 1] === "=") {
      // 避免 `===` 误匹配：左侧无 `=` 粘连
      if (i > 0 && line[i - 1] === "=") { i++; continue; }
      const openEnd = i + 2;
      const closeIdx = line.indexOf("==", openEnd);
      if (closeIdx === -1) break;
      // 避免 `===` 误匹配：右侧闭合 `==` 后无 `=` 粘连
      if (closeIdx + 2 < line.length && line[closeIdx + 2] === "=") { i = closeIdx + 1; continue; }
      const innerLen = closeIdx - openEnd;
      if (innerLen > 0) {
        results.push({
          innerFrom: baseOffset + openEnd,
          innerTo: baseOffset + closeIdx,
          markFrom: baseOffset + i,
          markTo: baseOffset + closeIdx,
        });
      }
      i = closeIdx + 2;
      continue;
    }
    i++;
  }
  return results;
}

/** 判断节点是否在数组区间内。 */
function inRanges(
  pos: number,
  ranges: readonly { from: number; to: number }[],
): boolean {
  for (const r of ranges) {
    if (pos >= r.from && pos < r.to) return true;
  }
  return false;
}

/** `![[...]]` 嵌入的图片扩展名（大小写不敏感）。 */
const EMBED_IMAGE_EXT_RE = /\.(png|jpe?g|gif|webp|svg|avif|bmp|ico)$/i;

/**
 * frontmatter 区间检测（纯文本判定，不依赖语法树——lezer 无 frontmatter 扩展）。
 * doc 第 1 行 trimEnd 后恰为 `---`，则向后找第一个 trimEnd 后为 `---`/`...` 的行
 * 作结束（最多扫描 300 行防退化）。找到则返回 [line1.from, endLine.to]。
 */
const FM_MAX_SCAN_LINES = 300;

function detectFrontmatterRange(doc: { lines: number; line(n: number): { text: string; from: number; to: number } }): { from: number; to: number } | null {
  if (doc.lines < 2) return null;
  const first = doc.line(1);
  if (first.text.trimEnd() !== "---") return null;
  const limit = Math.min(doc.lines, 1 + FM_MAX_SCAN_LINES);
  for (let n = 2; n <= limit; n++) {
    const t = doc.line(n).text.trimEnd();
    if (t === "---" || t === "...") {
      return { from: first.from, to: doc.line(n).to };
    }
  }
  return null;
}

/**
 * 从嵌入 target 提取纯文件名部分。`[[img.png|300]]` 的 target 经 bar 逻辑解析后
 * 为 `img.png`，但若出现 `[[dir/a.png|300x200]]` 等形态，尺寸后缀位于 label 侧；
 * 保险起见仍取 `|` 前段并 trim 后再判扩展名。
 */
function embedFileName(target: string): string {
  const bar = target.indexOf("|");
  return (bar === -1 ? target : target.slice(0, bar)).trim();
}

/**
 * 计算 [from, to) 区间内的 Live Preview 装饰 spec。
 * 调用方只对 `view.visibleRanges` 逐段调用；返回顺序为文档顺序（line 按行号，
 * 其余按 from 递增——同一 from 下 hide 先于 widget，视图层据此稳定建集）。
 */
export function computeLivePreviewSpecs(
  state: EditorState,
  from: number,
  to: number,
): DecoSpec[] {
  const specs: DecoSpec[] = [];
  const doc = state.doc;
  if (doc.length === 0) return specs;
  const lo = Math.max(0, from);
  const hi = Math.min(doc.length, to);
  if (lo >= hi) return specs;

  const tree = syntaxTree(state);
  // 语法树未就绪（无 markdown 解析器、空树等）→ 容错返回空，不抛错
  if (tree.length === 0) return specs;

  const sel = state.selection.ranges;
  const seenLine = new Set<string>();
  /**
   * 被光标触碰的标题节点区间：标题内行内构造（Strong/Emphasis/行内码/链接/图片）
   * 在该区间内一律抑制，使标题还原语义与 decorateAtx 一致。
   */
  const headingSuppressRanges: Array<{ from: number; to: number }> = [];

  // frontmatter 区间（纯文本判定）：无论折叠与否，其内部装饰一律抑制
  // （开头的 `---` 会被 lezer 误判为 HorizontalRule，结尾 `---` 被误判为 Setext）。
  const fmRange = detectFrontmatterRange(doc);
  // frontmatter 自身折叠/还原判定：选区触碰区间首末行即还原
  let fmCollapsed = false;
  if (fmRange) {
    const firstLine = doc.lineAt(fmRange.from);
    const lastLine = doc.lineAt(fmRange.to);
    if (!touchesLine(sel, firstLine.from, lastLine.to)) fmCollapsed = true;
  }

  function pushLine(line: number, cls: string): void {
    const key = `${line}:${cls}`;
    if (seenLine.has(key)) return;
    seenLine.add(key);
    specs.push({ kind: "line", line, cls });
  }

  function hide(fromPos: number, toPos: number): void {
    if (toPos > fromPos) specs.push({ kind: "hide", from: fromPos, to: toPos });
  }

  function isSpaceAt(pos: number): boolean {
    const ch = doc.sliceString(pos, pos + 1);
    return ch === " " || ch === "\t";
  }

  /**
   * ATX 标题整行装饰。开标记符 run + 其后空格隐藏；闭合 run（`#`/`##` 前通常
   * 带空格）连同分隔空格、行尾空格一并隐藏；无闭合时仅隐藏行尾空格。
   */
  function decorateAtx(node: SyntaxNodeRef, level: string): void {
    const line = doc.lineAt(node.from);
    if (touchesLine(sel, line.from, line.to)) return;
    pushLine(line.number, LP_CLASS[`h${level}` as keyof typeof LP_CLASS]);

    const marks = nodeMarks(node);
    if (marks.length === 0) return;
    const opening = marks[0];
    let contentStart = opening.to;
    while (contentStart < node.to && isSpaceAt(contentStart)) contentStart++;
    hide(opening.from, contentStart); // 开标记符 + 分隔空格

    if (marks.length > 1) {
      // 闭合 run 之前的空格一并隐藏
      const closing = marks[marks.length - 1];
      let hideFrom = closing.from;
      while (hideFrom > contentStart && isSpaceAt(hideFrom - 1)) hideFrom--;
      hide(hideFrom, node.to);
    } else {
      // 无闭合 run：隐藏行尾空格
      let end = node.to;
      while (end > contentStart && isSpaceAt(end - 1)) end--;
      if (end < node.to) hide(end, node.to);
    }
  }

  /** Setext 标题：正文行加 class，下划线行整体隐藏。返回 true 表示光标在正文行（阻止子节点行内装饰）。 */
  function decorateSetext(node: SyntaxNodeRef, level: string): boolean {
    const textLine = doc.lineAt(node.from);
    const underLine = doc.lineAt(Math.min(node.to, doc.length));
    const textTouched = touchesLine(sel, textLine.from, textLine.to);
    if (textTouched) return true;
    if (touchesLine(sel, underLine.from, underLine.to)) return true;
    pushLine(textLine.number, LP_CLASS[`h${level}` as keyof typeof LP_CLASS]);
    // 下划线行：自首字符（含可能的缩进）到节点末尾整段隐藏
    const marks = nodeMarks(node);
    if (marks.length > 0) {
      hide(marks[0].from, node.to);
    } else {
      hide(underLine.from, underLine.to);
    }
    return false;
  }

  // 围栏代码块 / 行内码区间：其内的 [[...]] 不视为 wikilink（lezer 树判定）
  const codeRanges: Array<{ from: number; to: number }> = [];
  // 表格区间：表格整体被 widget 替换时，内部的 wikilink/image 不单独渲染
  const tableRanges: Array<{ from: number; to: number }> = [];

  function nodeMarks(node: SyntaxNodeRef): readonly SyntaxNode[] {
    return node.node.getChildren("HeaderMark");
  }

  tree.iterate({
    from: lo,
    to: hi,
    enter(node) {
      const name = node.name;

      // frontmatter 内部装饰一律抑制（无论折叠与否）
      if (fmRange && inRanges(node.from, [{ from: fmRange.from, to: fmRange.to }])) {
        return false;
      }

      const atx = ATX_RE.exec(name);
      if (atx) {
        // 光标在标题行时 decorateAtx 不发射装饰；此时记录区间抑制行内子节点
        const line = doc.lineAt(node.from);
        if (touchesLine(sel, line.from, line.to)) {
          headingSuppressRanges.push({ from: node.from, to: node.to });
        }
        decorateAtx(node, atx[1]);
        return true; // 继续遍历子节点，但 touched 时被抑制
      }

      const setext = SETEXT_RE.exec(name);
      if (setext) {
        const touched = decorateSetext(node, setext[1]);
        if (touched) {
          // 光标在 Setext 正文行：记录区间抑制行内子节点
          headingSuppressRanges.push({ from: node.from, to: node.to });
        }
        return true;
      }

      if (name === "Blockquote") {
        const startLine = doc.lineAt(node.from).number;
        const endLine = doc.lineAt(Math.min(node.to, doc.length)).number;
        // callout 检测：仅检查引用块第一行（去掉 `>` 和空格后匹配 `[!type]`）
        let calloutInfo: { type: string; title: string; markFrom: number; markTo: number } | null = null;
        const firstLine = doc.line(startLine);
        const stripped = firstLine.text.replace(/^(\s*>)+\s*/, "");
        const calloutMatch = CALLOUT_RE.exec(stripped);
        if (calloutMatch) {
          const sanitized = sanitizeCalloutType(calloutMatch[1]);
          if (sanitized.length > 0) {
            const firstLineStart = firstLine.from + (firstLine.text.length - firstLine.text.replace(/^(\s*>)+\s*/, "").length);
            const markStart = firstLineStart + calloutMatch[0].indexOf("[");
            let markEnd = markStart + calloutMatch[0].length - (calloutMatch[2] ?? "").length;
            // 修剪 `[!type]` 标记与标题之间的空白：markEnd 只覆盖到 `+`/`-` 后缀
            markEnd = firstLineStart + calloutMatch[0].slice(0, calloutMatch[0].length - (calloutMatch[2] ?? "").length).trimEnd().length;
            const firstLineRevealed = touchesLine(sel, firstLine.from, firstLine.to);
            if (!firstLineRevealed) {
              calloutInfo = { type: sanitized, title: (calloutMatch[2] ?? "").trim(), markFrom: markStart, markTo: markEnd };
            }
          }
        }
        for (let ln = startLine; ln <= endLine; ln++) {
          const line = doc.line(ln);
          if (line.from > node.to) break;
          if (!touchesLine(sel, line.from, line.to)) {
            pushLine(ln, LP_CLASS.quote);
            // callout 类型类：该引用块所有未还原行都追加
            if (calloutInfo) pushLine(ln, `cm-lp-callout cm-lp-callout-${calloutInfo.type}`);
          }
        }
        // 非还原时：第一行 `[!type]` 标记 hide + callout widget spec
        if (calloutInfo) {
          hide(calloutInfo.markFrom, calloutInfo.markTo);
          specs.push({
            kind: "callout",
            from: calloutInfo.markFrom,
            to: calloutInfo.markTo,
            calloutType: calloutInfo.type,
            title: calloutInfo.title,
          });
        }
        return;
      }

      if (name === "HorizontalRule") {
        const line = doc.lineAt(node.from);
        if (!touchesLine(sel, line.from, line.to)) {
          specs.push({ kind: "hr", from: node.from, to: node.to });
        }
        return false;
      }

      if (name === "ListMark") {
        const line = doc.lineAt(node.from);
        if (!touchesLine(sel, line.from, line.to)) {
          specs.push({ kind: "mark", from: node.from, to: node.to, cls: LP_CLASS.listMark });
        }
        return;
      }

      if (name === "TaskMarker") {
        const line = doc.lineAt(node.from);
        if (!touchesLine(sel, line.from, line.to)) {
          const text = doc.sliceString(node.from, node.to);
          specs.push({
            kind: "checkbox",
            from: node.from,
            to: node.to,
            checked: text.toLowerCase().includes("x"),
          });
        }
        return false;
      }

      if (name === "FencedCode") {
        // 行背景装饰：编辑代码块时不还原（无文本增删，还原反而造成背景闪烁）
        codeRanges.push({ from: node.from, to: node.to });
        const startLine = doc.lineAt(node.from).number;
        const endLine = doc.lineAt(Math.min(node.to, doc.length)).number;
        for (let ln = startLine; ln <= endLine; ln++) {
          if (doc.line(ln).from > node.to) break;
          pushLine(ln, LP_CLASS.codeLine);
        }

        // 开围栏行 widget（``` + 可选语言标签）
        const openingMark = node.node.getChild("CodeMark");
        if (openingMark) {
          const langNode = node.node.getChild("CodeInfo");
          const lang = langNode ? doc.sliceString(langNode.from, langNode.to) : "";
          const openingLine = doc.lineAt(openingMark.from);
          if (!touchesLine(sel, openingLine.from, openingLine.to)) {
            specs.push({
              kind: "codeheader",
              from: openingLine.from,
              to: openingLine.to,
              lang,
            });
          }
        }

        // 闭围栏行 widget（最后一个 ```）
        const codeMarks = node.node.getChildren("CodeMark");
        if (codeMarks.length >= 2) {
          const closingMark = codeMarks[codeMarks.length - 1];
          const closingLine = doc.lineAt(closingMark.from);
          if (!touchesLine(sel, closingLine.from, closingLine.to)) {
            specs.push({
              kind: "codefooter",
              from: closingLine.from,
              to: closingLine.to,
            });
          }
        }

        return false;
      }

      if (
        name === "StrongEmphasis" ||
        name === "Emphasis" ||
        name === "Strikethrough" ||
        name === "InlineCode"
      ) {
        // 标题行内构造 suppress：若光标在标题行，不发射行内装饰
        if (inRanges(node.from, headingSuppressRanges)) return false;
        const marks = node.node
          .getChildren("EmphasisMark")
          .concat(
            node.node.getChildren("StrikethroughMark"),
            node.node.getChildren("CodeMark"),
          );
        if (name === "InlineCode") codeRanges.push({ from: node.from, to: node.to });
        if (marks.length > 0) {
          const pad = marks[0].to - marks[0].from;
          if (!touchesSpan(sel, node.from, node.to, pad)) {
            for (const mk of marks) hide(mk.from, mk.to);
          }
        }
        return;
      }

      // ── 图片：block（独占段落）/ inline ──────────────────────────────
      if (name === "Image") {
        if (inRanges(node.from, headingSuppressRanges)) return false;
        // 抑制 `![[...]]` 嵌入语法——由 wikilink 行扫描器统一处理。
        // 此时 Image 无 URL 子节点（rawSrc 为空），仍会发射 src="" 的误 spec。
        if (node.from < node.to) {
          const raw = doc.sliceString(node.from, node.to);
          if (
            raw.startsWith("![[")
            && raw.endsWith("]]")
            && raw.length >= 6
          ) return false;
        }
        // 跳过表格区间（表格 widget 已整体替换，内部不单独渲染图片）
        if (inRanges(node.from, tableRanges)) return false;

        const urlNode = node.node.getChild("URL");
        const rawSrc = urlNode ? doc.sliceString(urlNode.from, urlNode.to) : "";
        // 角括号 URL：<https://x.com/a b.png> —— 去掉外层尖括号
        const src = rawSrc.startsWith("<") && rawSrc.endsWith(">")
          ? rawSrc.slice(1, -1)
          : rawSrc;

        // alt 文本：![ 和 ] 之间
        const linkMarks = node.node.getChildren("LinkMark");
        const alt = linkMarks.length >= 2
          ? doc.sliceString(linkMarks[0].to, linkMarks[1].from)
          : "";

        const isAttachment = isAttachmentSrc(src);
        const resolvedSrc = isAttachment ? normalizeAttachmentSrc(src) : src;

        // 判断 block vs inline：
        // block = Image 是所属 Paragraph 的唯一内容（无其他文本/节点）
        const parent = node.node.parent;
        const isBlock = parent?.name === "Paragraph"
          && parent.from === node.from
          && parent.to === node.to;

        // 还原判定：block 用段落行，inline 用节点区间
        if (isBlock) {
          const line = doc.lineAt(node.from);
          if (touchesLine(sel, line.from, line.to)) return false;
        } else {
          if (touchesSpan(sel, node.from, node.to, 1)) return false;
        }

        specs.push({
          kind: "image",
          from: node.from,
          to: node.to,
          alt,
          src: resolvedSrc,
          block: isBlock,
        });
        return false;
      }

      // ── Markdown 链接 [text](url) ────────────────────────────────────
      if (name === "Link") {
        if (inRanges(node.from, headingSuppressRanges)) return false;
        if (inRanges(node.from, tableRanges)) return false;
        const urlNode = node.node.getChild("URL");
        if (!urlNode) return; // 引用式 [text][ref] 无 URL 子节点，跳过

        // 光标/选区进入该行 → 完整显示源码；否则整节点替换为链接 widget
        // （widget 覆盖 [text](url) 全区间，无需再隐藏标记符）
        const line = doc.lineAt(node.from);
        if (touchesLine(sel, line.from, line.to)) return false;

        const linkMarks = node.node.getChildren("LinkMark");
        const text = linkMarks.length >= 2
          ? doc.sliceString(linkMarks[0].to, linkMarks[1].from)
          : "";
        const url = doc.sliceString(urlNode.from, urlNode.to);
        specs.push({
          kind: "mdlink",
          from: node.from,
          to: node.to,
          text,
          url,
        });
        return false;
      }

      // ── Autolink <url>：隐藏尖括号（保留 URL 文本可见）─────────────────
      if (name === "Autolink") {
        if (inRanges(node.from, tableRanges)) return false;
        const line = doc.lineAt(node.from);
        if (touchesLine(sel, line.from, line.to)) return false;
        for (const lm of node.node.getChildren("LinkMark")) hide(lm.from, lm.to);
        return false;
      }

      // ── GFM 表格：整体替换为 HTML table widget ────────────────────────
      if (name === "Table") {
        const raw = doc.sliceString(node.from, node.to);
        const tableStartLine = doc.lineAt(node.from);
        const tableEndLine = doc.lineAt(Math.min(node.to, doc.length));
        if (touchesLine(sel, tableStartLine.from, tableEndLine.to)) {
          // 选区触碰表格行 → 还原为源码
          return;
        }
        tableRanges.push({ from: node.from, to: node.to });
        specs.push({ kind: "table", from: node.from, to: node.to, raw });
        return false;
      }

      return;
    },
  });

  // wikilink 行扫描：跳过围栏/行内码区间、表格区间和 frontmatter 区间
  const startLineNo = doc.lineAt(lo).number;
  const endLineNo = doc.lineAt(Math.min(hi, doc.length)).number;
  for (let ln = startLineNo; ln <= endLineNo; ln++) {
    const line = doc.line(ln);
    // 跳过 frontmatter 区间内的行
    if (fmRange && ln > 0) {
      const lineFrom = line.from;
      if (lineFrom >= fmRange.from && lineFrom <= fmRange.to) continue;
    }
    const spans = scanWikilinksInLine(line.text, line.from);
    for (const sp of spans) {
      if (sp.to <= lo || sp.from >= hi) continue;
      let inCode = false;
      for (const r of codeRanges) {
        if (sp.from < r.to && sp.to > r.from) {
          inCode = true;
          break;
        }
      }
      if (inCode) continue;
      // 表格区间也跳过（表格 widget 整体替换时不渲染内部 wikilink）
      if (inRanges(sp.from, tableRanges)) continue;

      // ── `![[...]]` 嵌入语法处理 ──────────────────────────────
      if (sp.embed) {
        // from 向前扩 1 吞掉 `!`，touchesSpan 判定也用扩后区间
        const embFrom = sp.from - 1;
        const embTo = sp.to;

        if (touchesSpan(sel, embFrom, embTo, 2)) continue;

        const fileName = embedFileName(sp.target);
        const isImage = EMBED_IMAGE_EXT_RE.test(fileName);

        if (isImage) {
          const src = fileName.startsWith("assets/") ? fileName : `assets/${fileName}`;
          // alt 用文件名（去扩展名）：剥掉可能的目录前缀
          const baseName = fileName.includes("/") ? fileName.slice(fileName.lastIndexOf("/") + 1) : fileName;
          const alt = baseName.replace(/\.[^.]+$/, "");
          // block = 独占段落（仅含 `![[...]]`，无前后文本）
          const isBlock = line.from === embFrom && line.to === embTo;
          specs.push({
            kind: "image",
            from: embFrom,
            to: embTo,
            alt,
            src,
            block: isBlock,
          });
        } else {
          // 非图片嵌入（笔记）：暂退化为 wikilink
          specs.push({
            kind: "wikilink",
            from: embFrom,
            to: embTo,
            target: sp.target,
            label: sp.label,
          });
        }
        continue;
      }
      // ── 常规 wikilink ─────────────────────────────────────────

      // 光标/选区进入（含 [[ / ]] 两侧各 2 字符外扩）即还原为源码
      if (touchesSpan(sel, sp.from, sp.to, 2)) continue;
      specs.push({
        kind: "wikilink",
        from: sp.from,
        to: sp.to,
        target: sp.target,
        label: sp.label,
      });
    }
  }

  // ── `==highlight==` 行扫描：跳过围栏/行内码区间、表格区间和 frontmatter 区间 ──
  for (let ln = startLineNo; ln <= endLineNo; ln++) {
    const line = doc.line(ln);
    if (fmRange && ln > 0) {
      if (line.from >= fmRange.from && line.from <= fmRange.to) continue;
    }
    // 跳过表格区间内的行
    if (inRanges(line.from, tableRanges)) continue;
    const hlMatches = scanHighlightsInLine(line.text, line.from);
    for (const hl of hlMatches) {
      if (hl.innerTo <= lo || hl.innerFrom >= hi) continue;
      // 跳过代码区间内的高亮
      let inCode = false;
      for (const r of codeRanges) {
        if (hl.innerFrom < r.to && hl.innerTo > r.from) { inCode = true; break; }
      }
      if (inCode) continue;
      // 还原：光标触碰内容区间（pad=2 包含两侧 `==` 标记）时不渲染
      if (touchesSpan(sel, hl.innerFrom, hl.innerTo, 2)) continue;
      // 隐藏两个 `==` 标记
      hide(hl.markFrom, hl.markFrom + 2);
      hide(hl.markTo, hl.markTo + 2);
      // 内容区间加 highlight mark class
      specs.push({ kind: "highlight", from: hl.innerFrom, to: hl.innerTo, markFrom: hl.markFrom, markTo: hl.markTo });
    }
  }

  // ── frontmatter 折叠：选区不触碰时发射 spec ──────────────────────
  if (fmRange && fmCollapsed) {
    const firstLine = doc.lineAt(fmRange.from);
    const lastLine = doc.lineAt(fmRange.to);
    // 仅在可见区间内才发射
    if (fmRange.from < hi && fmRange.to > lo) {
      const propCount = Math.max(0, lastLine.number - firstLine.number - 1);
      specs.push({
        kind: "frontmatter",
        from: fmRange.from,
        to: fmRange.to,
        propCount,
      });
    }
  }

  return specs;
}
