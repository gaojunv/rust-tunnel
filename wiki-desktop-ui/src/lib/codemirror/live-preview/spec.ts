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
  | { kind: "codefooter"; from: number; to: number };

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

  /** Setext 标题：正文行加 class，下划线行整体隐藏。 */
  function decorateSetext(node: SyntaxNodeRef, level: string): void {
    const textLine = doc.lineAt(node.from);
    const underLine = doc.lineAt(Math.min(node.to, doc.length));
    if (touchesLine(sel, textLine.from, textLine.to)) return;
    if (touchesLine(sel, underLine.from, underLine.to)) return;
    pushLine(textLine.number, LP_CLASS[`h${level}` as keyof typeof LP_CLASS]);
    // 下划线行：自首字符（含可能的缩进）到节点末尾整段隐藏
    const marks = nodeMarks(node);
    if (marks.length > 0) {
      hide(marks[0].from, node.to);
    } else {
      hide(underLine.from, underLine.to);
    }
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

      const atx = ATX_RE.exec(name);
      if (atx) {
        decorateAtx(node, atx[1]);
        return false;
      }

      const setext = SETEXT_RE.exec(name);
      if (setext) {
        decorateSetext(node, setext[1]);
        return false;
      }

      if (name === "Blockquote") {
        const startLine = doc.lineAt(node.from).number;
        const endLine = doc.lineAt(Math.min(node.to, doc.length)).number;
        for (let ln = startLine; ln <= endLine; ln++) {
          const line = doc.line(ln);
          if (line.from > node.to) break;
          if (!touchesLine(sel, line.from, line.to)) pushLine(ln, LP_CLASS.quote);
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

  // wikilink 行扫描：跳过围栏/行内码区间和表格区间（上一步收集的 lezer 区间）
  const startLineNo = doc.lineAt(lo).number;
  const endLineNo = doc.lineAt(Math.min(hi, doc.length)).number;
  for (let ln = startLineNo; ln <= endLineNo; ln++) {
    const line = doc.line(ln);
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

  return specs;
}
