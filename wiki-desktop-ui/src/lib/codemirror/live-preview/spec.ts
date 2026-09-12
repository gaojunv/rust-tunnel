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
  codeLastLine: "cm-lp-codeblock-last",
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
 * - `mathblock`: 块级数学 `$$...$$` 替换为 KaTeX widget
 * - `mathinline`: 行内数学 `$...$` 替换为 KaTeX widget
 * - `bullet`: 无序列表标记 `-`/`+`/`*` 替换为圆点 widget
 */
export type DecoSpec =
  | { kind: "line"; line: number; cls: string }
  | { kind: "mark"; from: number; to: number; cls: string }
  | { kind: "hide"; from: number; to: number }
  | { kind: "checkbox"; from: number; to: number; checked: boolean }
  | { kind: "hr"; from: number; to: number }
  | { kind: "wikilink"; from: number; to: number; target: string; label: string }
  | { kind: "image"; from: number; to: number; alt: string; src: string; block: boolean; width?: number; height?: number }
  | { kind: "mdlink"; from: number; to: number; text: string; url: string }
  | { kind: "table"; from: number; to: number; raw: string }
  | { kind: "codeheader"; from: number; to: number; lang: string }
  | { kind: "codefooter"; from: number; to: number }
  | { kind: "frontmatter"; from: number; to: number; propCount: number }
  | { kind: "callout"; from: number; to: number; calloutType: string; title: string }
  | { kind: "highlight"; from: number; to: number; markFrom: number; markTo: number }
  | { kind: "mathblock"; from: number; to: number; tex: string }
  | { kind: "mathinline"; from: number; to: number; tex: string }
  | { kind: "bullet"; from: number; to: number };

type SelRange = { from: number; to: number };

/** 空选区集（zoned 模式：全部 reveal 判定恒为"未触碰"）。 */
const EMPTY_RANGES: readonly SelRange[] = [];

/** 空判定区集（默认参数用，避免每次分配）。 */
const EMPTY_ZONES: readonly RevealZone[] = [];

/**
 * Reveal 判定区（选择无关的纯位置区间；span 的 pad 在构造时已折入）。
 * - `neg`: spec 可见要求选区与其中每一个都不相交（"任一相交即还原"，
 *   对应具体计算中多个 `touches…` 守卫的"都不命中才发射"）。
 * - `pos`: spec 可见要求选区与其中每一个都相交。仅行扫描使用：行扫描会逐行
 *   经过表格行，而具体计算中 `tableRanges` 只收录未还原表格——表格还原时其
 *   行落到扫描中、只受自身守卫约束。因此扫描命中的 spec 要求含它的表格
 *   全部处于还原态（被选区触碰），否则（表格未还原）扫描行会被整行跳过。
 */
export interface RevealZone {
  from: number;
  to: number;
}

/** 带 reveal 判定区的 spec（`index.ts` 按选区过滤后得到与具体计算一致的子集）。 */
export interface ZonedSpec {
  spec: DecoSpec;
  neg: RevealZone[];
  pos: RevealZone[];
}

/** 区与选区集相交判定（`touchesLine` / `touchesSpan` 的公共形状）。 */
export function zoneTouchedByRanges(
  zone: RevealZone,
  ranges: readonly SelRange[],
): boolean {
  for (const r of ranges) {
    if (r.from <= zone.to && r.to >= zone.from) return true;
  }
  return false;
}

/** zoned spec 在给定选区下是否可见（与具体计算的发射条件等价）。 */
export function isZonedSpecVisible(
  zs: ZonedSpec,
  ranges: readonly SelRange[],
): boolean {
  for (const z of zs.neg) {
    if (zoneTouchedByRanges(z, ranges)) return false;
  }
  for (const z of zs.pos) {
    if (!zoneTouchedByRanges(z, ranges)) return false;
  }
  return true;
}

/** 按选区过滤 zoned specs（保持原有顺序，结果与具体计算一致）。 */
export function filterZonedSpecs(
  zoned: readonly ZonedSpec[],
  ranges: readonly SelRange[],
): DecoSpec[] {
  const out: DecoSpec[] = [];
  for (const zs of zoned) {
    if (isZonedSpecVisible(zs, ranges)) out.push(zs.spec);
  }
  return out;
}

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
 * 图片尺寸后缀 `|W` / `|WxH`（必须带 `|` 前缀，如 `alt|300`）。
 * 返回宽度/高度与去掉后缀的前段文本。
 */
function splitImageSize(text: string): { width: number; height?: number; rest: string } | null {
  const m = /\|(\d+)(?:x(\d+))?$/.exec(text);
  if (!m) return null;
  return {
    width: Number(m[1]),
    height: m[2] !== undefined ? Number(m[2]) : undefined,
    rest: text.slice(0, m.index),
  };
}

/**
 * 纯数字尺寸 `W` / `WxH`（无 `|` 前缀）。
 * 用于 `![[img.png|300]]` 的 label 侧（target 经 `|` 切分后已不含尺寸段，
 * 尺寸落在 label 上，如 label === "300" / "300x200"）。
 */
function parseBareSize(text: string): { width: number; height?: number } | null {
  const m = /^(\d+)(?:x(\d+))?$/.exec(text.trim());
  if (!m) return null;
  return {
    width: Number(m[1]),
    height: m[2] !== undefined ? Number(m[2]) : undefined,
  };
}

/**
 * 引用式链接定义表惰性构建：遍历 doc 全文，按行匹配
 * `^\s{0,3}\[([^\]]+)\]:\s*(\S+)(?:\s+["'(].*)?$`，返回 Map<小写key, url>。
 */
function buildRefDefs(doc: { lines: number; line(n: number): { text: string } }): Map<string, string> {
  const map = new Map<string, string>();
  const re = /^\s{0,3}\[([^\]]+)\]:\s*(\S+)(?:\s+["'(].*)?$/;
  for (let n = 1; n <= doc.lines; n++) {
    const m = re.exec(doc.line(n).text);
    if (m) {
      map.set(m[1].toLowerCase().trim(), m[2]);
    }
  }
  return map;
}

/**
 * 扫描单行中非代码区间的行内数学 `$...$`。
 * - `$$` 不算行内（跳过连续 `$$`）
 * - 开 `$` 后紧邻非空白，闭 `$` 前紧邻非空白
 * - 闭 `$` 后不能紧跟数字
 * - 不在 codeSpan 内
 * 返回匹配数组，各条目 from/to 覆盖两个 `$` 符号本身。
 */
function scanInlineMathInLine(
  line: string,
  baseOffset: number,
): Array<{ from: number; to: number; tex: string }> {
  const results: Array<{ from: number; to: number; tex: string }> = [];

  // 收集行内代码区间（反引号对）
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
  while (i < line.length) {
    if (line[i] !== "$") { i++; continue; }
    // 跳过代码区间内的 $
    if (inCode(i)) { i++; continue; }
    // $$ 不算行内——跳过连续 $$
    if (i + 1 < line.length && line[i + 1] === "$") { i += 2; continue; }
    // 开 $ 后紧邻非空白
    if (i + 1 >= line.length || /\s/.test(line[i + 1])) { i++; continue; }
    // 向后找闭 $
    let j = i + 2;
    let found = false;
    while (j < line.length) {
      if (line[j] !== "$") { j++; continue; }
      // 跳过代码区间
      if (inCode(j)) { j++; continue; }
      // 闭 $ 前紧邻非空白
      if (/\s/.test(line[j - 1])) { j++; continue; }
      // 闭 $ 后不能紧跟数字
      if (j + 1 < line.length && /\d/.test(line[j + 1])) { j++; continue; }
      found = true;
      break;
    }
    if (!found) { i++; continue; }
    const tex = line.slice(i + 1, j);
    if (tex.length > 0) {
      results.push({
        from: baseOffset + i,
        to: baseOffset + j + 1,
        tex,
      });
    }
    i = j + 1;
  }
  return results;
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
  return computeInner(state, from, to, null).specs;
}

/**
 * 以选择无关形式计算全文装饰：返回全部"可能" spec，各附 reveal 判定区。
 * 调用方按当前选区 `filterZonedSpecs` 过滤，得到与
 * `computeLivePreviewSpecs(state, 0, doc.length)` 完全一致的子集
 * （顺序一致）。同一文档内容下结果可缓存，选区变化时只重做过滤。
 */
export function computeZonedSpecs(state: EditorState): ZonedSpec[] {
  return computeInner(state, 0, state.doc.length, { zoned: [] }).zoned ?? [];
}

interface ZonedSink {
  zoned: ZonedSpec[];
}

interface ComputeResult {
  specs: DecoSpec[];
  zoned: ZonedSpec[] | null;
}

function computeInner(
  state: EditorState,
  from: number,
  to: number,
  sink: ZonedSink | null,
): ComputeResult {
  const zonedMode = sink !== null;
  const specs: DecoSpec[] = [];
  const zoned: ZonedSpec[] = sink?.zoned ?? [];
  const doc = state.doc;
  if (doc.length === 0) return { specs, zoned: zonedMode ? zoned : null };
  const lo = Math.max(0, from);
  const hi = Math.min(doc.length, to);
  if (lo >= hi) return { specs, zoned: zonedMode ? zoned : null };

  const tree = syntaxTree(state);
  // 语法树未就绪（无 markdown 解析器、空树等）→ 容错返回空，不抛错
  if (tree.length === 0) return { specs, zoned: zonedMode ? zoned : null };

  // zoned 模式用空选区：全部 reveal 判定恒为"未触碰"，发射全集并记录判定区；
  // 具体模式用真实选区，行为与旧实现逐行一致。
  const sel: readonly SelRange[] = zonedMode ? EMPTY_RANGES : state.selection.ranges;
  const seenLine = new Set<string>();

  /**
   * 标题抑制区栈：进入 ATX/Setext 节点时压入其行区（zoned 模式），
   * 后续同子树内发射的 spec 自动继承——对应具体计算中
   * `headingSuppressRanges` 的合取抑制（标题不可嵌套，栈即抑制区）。
   */
  const negStack: RevealZone[] = [];

  /**
   * 发射一个 spec（具体模式直推数组；zoned 模式附判定区快照）。
   * `ignoreStack`：旧逻辑不检查 `headingSuppressRanges` 的发射点
   * （Autolink 一系）置 true，不继承标题抑制栈。
   * `pos`：仅行扫描使用（表格还原态要求），树处理函数恒为空。
   */
  function emit(
    spec: DecoSpec,
    neg: readonly RevealZone[],
    pos: readonly RevealZone[] = EMPTY_ZONES,
    ignoreStack = false,
  ): void {
    if (zonedMode) {
      zoned.push({
        spec,
        neg: ignoreStack ? [...neg] : [...negStack, ...neg],
        pos: [...pos],
      });
    } else {
      specs.push(spec);
    }
  }

  /**
   * 条件发射：具体模式下 `hit` 为真时跳过（还原）；zoned 模式下恒发射，
   * 并把判定区记入栈快照。调用形状对应旧代码的
   * `if (touches...(sel, ...)) return/continue; ...发射...`。
   */
  function guarded(
    neg: readonly RevealZone[],
    hit: boolean,
    fn: () => void,
  ): void {
    if (zonedMode) {
      for (const z of neg) negStack.push(z);
      try {
        fn();
      } finally {
        negStack.length -= neg.length;
      }
    } else if (!hit) {
      fn();
    }
  }
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

  function pushLine(
    line: number,
    cls: string,
    neg: readonly RevealZone[] = EMPTY_ZONES,
  ): void {
    const key = `${line}:${cls}`;
    if (seenLine.has(key)) return;
    seenLine.add(key);
    emit({ kind: "line", line, cls }, neg);
  }

  function hide(
    fromPos: number,
    toPos: number,
    neg: readonly RevealZone[] = EMPTY_ZONES,
    pos: readonly RevealZone[] = EMPTY_ZONES,
    ignoreStack = false,
  ): void {
    if (toPos > fromPos) emit({ kind: "hide", from: fromPos, to: toPos }, neg, pos, ignoreStack);
  }

  /** 行区间判定区（对应 `touchesLine(sel, lineFrom, lineTo)`）。 */
  function lineZone(lineFrom: number, lineTo: number): RevealZone {
    return { from: lineFrom, to: lineTo };
  }

  /** 行内区间判定区，pad 已折入（对应 `touchesSpan(sel, from, to, pad)`）。 */
  function spanZone(fromPos: number, toPos: number, pad: number): RevealZone {
    return { from: fromPos - pad, to: toPos + pad };
  }

  /**
   * 包含给定位置的表格判定区。只用于行扫描：行扫描会逐行经过表格行，
   * 而 `tableRanges` 只收录未还原表格——命中行的 spec 要求含它的表格
   * 全部处于还原态（见 `RevealZone` 的 pos 说明）。语法树遍历在 zoned
   * 下不进入表格子树，树处理函数无需调用。具体模式恒返回空（走即时判定）。
   */
  function tablePosAt(pos: number): readonly RevealZone[] {
    if (!zonedMode) return EMPTY_ZONES;
    const out: RevealZone[] = [];
    for (const r of tableRanges) {
      if (pos >= r.from && pos < r.to) out.push({ from: r.from, to: r.to });
    }
    return out;
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
    const lz: RevealZone = lineZone(line.from, line.to);
    guarded([lz], touchesLine(sel, line.from, line.to), () => {
      pushLine(line.number, LP_CLASS[`h${level}` as keyof typeof LP_CLASS], [lz]);

      const marks = nodeMarks(node);
      if (marks.length === 0) return;
      const opening = marks[0];
      let contentStart = opening.to;
      while (contentStart < node.to && isSpaceAt(contentStart)) contentStart++;
      hide(opening.from, contentStart, [lz]); // 开标记符 + 分隔空格

      if (marks.length > 1) {
        // 闭合 run 之前的空格一并隐藏
        const closing = marks[marks.length - 1];
        let hideFrom = closing.from;
        while (hideFrom > contentStart && isSpaceAt(hideFrom - 1)) hideFrom--;
        hide(hideFrom, node.to, [lz]);
      } else {
        // 无闭合 run：隐藏行尾空格
        let end = node.to;
        while (end > contentStart && isSpaceAt(end - 1)) end--;
        if (end < node.to) hide(end, node.to, [lz]);
      }
    });
  }

  /** Setext 标题：正文行加 class，下划线行整体隐藏。返回 true 表示光标在正文行（阻止子节点行内装饰）。 */
  function decorateSetext(node: SyntaxNodeRef, level: string): boolean {
    const textLine = doc.lineAt(node.from);
    const underLine = doc.lineAt(Math.min(node.to, doc.length));
    const zones: RevealZone[] = [
      lineZone(textLine.from, textLine.to),
      lineZone(underLine.from, underLine.to),
    ];
    const textTouched = touchesLine(sel, textLine.from, textLine.to);
    if (textTouched) return true;
    if (touchesLine(sel, underLine.from, underLine.to)) return true;
    pushLine(textLine.number, LP_CLASS[`h${level}` as keyof typeof LP_CLASS], zones);
    // 下划线行：自首字符（含可能的缩进）到节点末尾整段隐藏
    const marks = nodeMarks(node);
    if (marks.length > 0) {
      hide(marks[0].from, node.to, zones);
    } else {
      hide(underLine.from, underLine.to, zones);
    }
    return false;
  }

  // 围栏代码块 / 行内码区间：其内的 [[...]] 不视为 wikilink（lezer 树判定）
  const codeRanges: Array<{ from: number; to: number }> = [];
  // 表格区间：表格整体被 widget 替换时，内部的 wikilink/image 不单独渲染
  const tableRanges: Array<{ from: number; to: number }> = [];
  // 引用式链接定义表（惰性构建：第一次遇到无 URL 的 Link 节点时填充）
  let refDefs: Map<string, string> | null = null;

  function nodeMarks(node: SyntaxNodeRef): readonly SyntaxNode[] {
    return node.node.getChildren("HeaderMark");
  }

  tree.iterate({
    from: lo,
    to: hi,
    enter(node) {
      const name = node.name;

      // frontmatter 内部装饰一律抑制（无论折叠与否）。
      // 注意必须用「完全包含」判定：fmRange 从文档 0 偏移开始，Document 根
      // 节点的 from=0 也落在区间内——用 inRanges(node.from) 会把整棵树吞掉。
      if (fmRange && node.from >= fmRange.from && node.to <= fmRange.to) {
        return false;
      }

      const atx = ATX_RE.exec(name);
      if (atx) {
        // 光标在标题行时 decorateAtx 不发射装饰；此时记录区间抑制行内子节点
        const line = doc.lineAt(node.from);
        if (touchesLine(sel, line.from, line.to)) {
          headingSuppressRanges.push({ from: node.from, to: node.to });
        }
        // zoned 模式：Autolink 不检查标题抑制（旧逻辑亦然），只对检查抑制的
        // 行内构造（Strong/Emphasis/Strikethrough/InlineCode/Image/Link）
        // 压栈记录标题行判定区。
        if (zonedMode) negStack.push(lineZone(line.from, line.to));
        decorateAtx(node, atx[1]);
        return true; // 继续遍历子节点，但 touched 时被抑制
      }

      const setext = SETEXT_RE.exec(name);
      if (setext) {
        const textLine = doc.lineAt(node.from);
        const underLine = doc.lineAt(Math.min(node.to, doc.length));
        if (zonedMode) {
          // 同 ATX：只抑制检查 headingSuppressRanges 的行内构造；
          // Autolink 的旧逻辑不检查，此处压栈需排除它（见 Autolink 处理）。
          negStack.push(
            lineZone(textLine.from, textLine.to),
            lineZone(underLine.from, underLine.to),
          );
        }
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
            // 首行被还原时整块 callout 语义消失（类型类/标记 hide/spec 都不发射）；
            // zoned 模式下 sel 为空、判定恒为未触碰，行为与具体模式一致。
            const firstLineRevealed = touchesLine(sel, firstLine.from, firstLine.to);
            if (!firstLineRevealed) {
              calloutInfo = { type: sanitized, title: (calloutMatch[2] ?? "").trim(), markFrom: markStart, markTo: markEnd };
            }
          }
        }
        const firstLineZone: RevealZone = lineZone(firstLine.from, firstLine.to);
        for (let ln = startLine; ln <= endLine; ln++) {
          const line = doc.line(ln);
          if (line.from > node.to) break;
          const lz: RevealZone = lineZone(line.from, line.to);
          guarded([lz], touchesLine(sel, line.from, line.to), () => {
            pushLine(ln, LP_CLASS.quote, [lz]);
            // callout 类型类：首行未还原时，该引用块所有未还原行都追加；
            // 首行还原则整块无 callout 语义（calloutInfo 为 null）。
            // neg 区取"首行 × 本行"合取（任一还原即消失）。
            if (calloutInfo) {
              pushLine(
                ln,
                `cm-lp-callout cm-lp-callout-${calloutInfo.type}`,
                ln === startLine ? [lz] : [firstLineZone, lz],
              );
            }
          });
        }
        // 非还原时：第一行 `[!type]` 标记 hide + callout widget spec
        if (calloutInfo) {
          hide(calloutInfo.markFrom, calloutInfo.markTo, [firstLineZone]);
          emit({
            kind: "callout",
            from: calloutInfo.markFrom,
            to: calloutInfo.markTo,
            calloutType: calloutInfo.type,
            title: calloutInfo.title,
          }, [firstLineZone]);
        }
        return;
      }

      if (name === "HorizontalRule") {
        const line = doc.lineAt(node.from);
        const lz: RevealZone = lineZone(line.from, line.to);
        guarded([lz], touchesLine(sel, line.from, line.to), () => {
          emit({ kind: "hr", from: node.from, to: node.to }, [lz]);
        });
        return false;
      }

      if (name === "ListMark") {
        const line = doc.lineAt(node.from);
        const lz: RevealZone = lineZone(line.from, line.to);
        guarded([lz], touchesLine(sel, line.from, line.to), () => {
          const markText = doc.sliceString(node.from, node.to).trim();
          // 无序列表标记：`-`、`+`、`*`（单字符且不含数字）
          if (markText.length === 1 && "-+*".includes(markText)) {
            emit({ kind: "bullet", from: node.from, to: node.to }, [lz]);
          } else {
            emit({ kind: "mark", from: node.from, to: node.to, cls: LP_CLASS.listMark }, [lz]);
          }
        });
        return;
      }

      if (name === "TaskMarker") {
        const line = doc.lineAt(node.from);
        const lz: RevealZone = lineZone(line.from, line.to);
        guarded([lz], touchesLine(sel, line.from, line.to), () => {
          const text = doc.sliceString(node.from, node.to);
          emit({
            kind: "checkbox",
            from: node.from,
            to: node.to,
            checked: text.toLowerCase().includes("x"),
          }, [lz]);
        });
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

        // 代码块末行圆角 class：最后一个内容行（闭围栏前一行）
        const codeMarks = node.node.getChildren("CodeMark");
        let lastContentLine: number;
        if (codeMarks.length >= 2) {
          // 有闭围栏：末行 = 闭围栏行号 - 1
          const closingMark = codeMarks[codeMarks.length - 1];
          const closingLineNum = doc.lineAt(closingMark.from).number;
          lastContentLine = closingLineNum - 1;
        } else {
          // 无闭围栏（未闭合）：末行 = endLine
          lastContentLine = endLine;
        }
        if (lastContentLine >= startLine && lastContentLine <= endLine) {
          const lastContentLineObj = doc.line(lastContentLine);
          if (lastContentLineObj.from <= node.to) {
            pushLine(lastContentLine, LP_CLASS.codeLastLine);
          }
        }

        // 开围栏行 widget（``` + 可选语言标签）
        const openingMark = node.node.getChild("CodeMark");
        if (openingMark) {
          const langNode = node.node.getChild("CodeInfo");
          const lang = langNode ? doc.sliceString(langNode.from, langNode.to) : "";
          const openingLine = doc.lineAt(openingMark.from);
          const olz: RevealZone = lineZone(openingLine.from, openingLine.to);
          guarded([olz], touchesLine(sel, openingLine.from, openingLine.to), () => {
            emit({
              kind: "codeheader",
              from: openingLine.from,
              to: openingLine.to,
              lang,
            }, [olz]);
          });
        }

        // 闭围栏行 widget（最后一个 ```）
        if (codeMarks.length >= 2) {
          const closingMark = codeMarks[codeMarks.length - 1];
          const closingLine = doc.lineAt(closingMark.from);
          const clz: RevealZone = lineZone(closingLine.from, closingLine.to);
          guarded([clz], touchesLine(sel, closingLine.from, closingLine.to), () => {
            emit({
              kind: "codefooter",
              from: closingLine.from,
              to: closingLine.to,
            }, [clz]);
          });
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
          const sz: RevealZone = spanZone(node.from, node.to, pad);
          guarded([sz], touchesSpan(sel, node.from, node.to, pad), () => {
            for (const mk of marks) hide(mk.from, mk.to, [sz]);
          });
        }
        return;
      }

      // ── 图片：block（独占段落）/ inline ──────────────────────────────
      // 注意：表格抑制是"含该节点即跳过"（return false，不因子节点还原
      // 而恢复）——zoned 下同样记为"含该表格即抑制"：neg 区取并集。
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
        // 跳过表格区间（表格 widget 已整体替换，内部不单独渲染图片）。
        // zoned 下不进入表格子树，能访问到此处的节点必不在任何表格内，无需记区。
        if (inRanges(node.from, tableRanges)) return false;

        const urlNode = node.node.getChild("URL");
        const rawSrc = urlNode ? doc.sliceString(urlNode.from, urlNode.to) : "";
        // 角括号 URL：<https://x.com/a b.png> —— 去掉外层尖括号
        const src = rawSrc.startsWith("<") && rawSrc.endsWith(">")
          ? rawSrc.slice(1, -1)
          : rawSrc;

        // alt 文本：![ 和 ] 之间
        const linkMarks = node.node.getChildren("LinkMark");
        let alt = linkMarks.length >= 2
          ? doc.sliceString(linkMarks[0].to, linkMarks[1].from)
          : "";

        // 图片尺寸语法：alt 末尾 `|W` 或 `|WxH`（去掉后缀，前段作 alt）
        let imgWidth: number | undefined;
        let imgHeight: number | undefined;
        const sizeInfo = splitImageSize(alt);
        if (sizeInfo) {
          imgWidth = sizeInfo.width;
          imgHeight = sizeInfo.height;
          alt = sizeInfo.rest.trimEnd();
        }

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
          const lz: RevealZone = lineZone(line.from, line.to);
          guarded([lz], touchesLine(sel, line.from, line.to), () => {
            emit({
              kind: "image",
              from: node.from,
              to: node.to,
              alt,
              src: resolvedSrc,
              block: isBlock,
              ...(imgWidth !== undefined ? { width: imgWidth, height: imgHeight } : {}),
            }, [lz]);
          });
        } else {
          const sz: RevealZone = spanZone(node.from, node.to, 1);
          guarded([sz], touchesSpan(sel, node.from, node.to, 1), () => {
            emit({
              kind: "image",
              from: node.from,
              to: node.to,
              alt,
              src: resolvedSrc,
              block: isBlock,
              ...(imgWidth !== undefined ? { width: imgWidth, height: imgHeight } : {}),
            }, [sz]);
          });
        }
        return false;
      }

      // ── Markdown 链接 [text](url) ────────────────────────────────────
      // 注意：zoned 下不进入表格子树，能访问到此处的节点必不在任何表格内，无需记区。
      if (name === "Link") {
        if (inRanges(node.from, headingSuppressRanges)) return false;
        if (inRanges(node.from, tableRanges)) return false;
        const urlNode = node.node.getChild("URL");

        // 引用式链接 [text][ref]：无 URL 子节点，用 LinkLabel 作 ref key 查定义表
        if (!urlNode) {
          const linkMarks = node.node.getChildren("LinkMark");
          const text = linkMarks.length >= 2
            ? doc.sliceString(linkMarks[0].to, linkMarks[1].from)
            : "";
          if (!text) return;

          // ref key 取自 LinkLabel 子节点（`[ref]` 去括号）；缺省/空时回退用 text
          // （折叠式 `[text][]` 即以 text 作 key，符合 CommonMark 语义）
          const refLabelNode = node.node.getChild("LinkLabel");
          let refKey = text;
          if (refLabelNode) {
            const rawRef = doc.sliceString(refLabelNode.from, refLabelNode.to);
            const stripped = rawRef.startsWith("[") && rawRef.endsWith("]")
              ? rawRef.slice(1, -1)
              : rawRef;
            if (stripped.trim()) refKey = stripped;
          }

          // 惰性构建引用定义表
          if (refDefs === null) refDefs = buildRefDefs(doc);
          const url = refDefs.get(refKey.toLowerCase().trim());
          if (!url) return; // 未找到定义，不发射

          // 光标/选区进入该行 → 完整显示源码
          const line = doc.lineAt(node.from);
          const lz: RevealZone = lineZone(line.from, line.to);
          guarded([lz], touchesLine(sel, line.from, line.to), () => {
            emit({
              kind: "mdlink",
              from: node.from,
              to: node.to,
              text,
              url,
            }, [lz]);
          });
          return false;
        }

        // 光标/选区进入该行 → 完整显示源码；否则整节点替换为链接 widget
        // （widget 覆盖 [text](url) 全区间，无需再隐藏标记符）
        const line = doc.lineAt(node.from);
        const lz: RevealZone = lineZone(line.from, line.to);
        guarded([lz], touchesLine(sel, line.from, line.to), () => {
          const linkMarks = node.node.getChildren("LinkMark");
          const text = linkMarks.length >= 2
            ? doc.sliceString(linkMarks[0].to, linkMarks[1].from)
            : "";
          const url = doc.sliceString(urlNode.from, urlNode.to);
          emit({
            kind: "mdlink",
            from: node.from,
            to: node.to,
            text,
            url,
          }, [lz]);
        });
        return false;
      }

      // ── Autolink <url>：隐藏尖括号（保留 URL 文本可见）─────────────────
      // 注意：旧逻辑不检查 headingSuppressRanges——标题/集标题还原只通过
      // 行判定隐式抑制（光标在标题行必触碰该行）；zoned 下同样只记行区。
      // zoned 下不进入表格子树，能访问到此处的节点必不在任何表格内，无需记区。
      if (name === "Autolink") {
        if (inRanges(node.from, tableRanges)) return false;
        const line = doc.lineAt(node.from);
        const lz: RevealZone = lineZone(line.from, line.to);
        guarded([lz], touchesLine(sel, line.from, line.to), () => {
          for (const lm of node.node.getChildren("LinkMark")) {
            hide(lm.from, lm.to, [lz], EMPTY_ZONES, true);
          }
        });
        return false;
      }

      // ── GFM 表格：整体替换为 HTML table widget ────────────────────────
      // 注意：进入子树与否取决于表格自身是否还原（还原 → `return` 继续遍历
      // 子节点，行内构造各自判定；未还原 → `return false` 跳过子树）。
      // zoned 下 sel 为空、表格恒未还原：恒收录自身 spec 且不进入子树。
      // 树内构造（Image/Link/Autolink）在表格还原、遍历进入子树时发射，
      // 此时其发射条件与"表格内"无关（`inRanges` 即时判定恒为假）——
      // zoned 下复刻为"表格还原时重走子树"：见 computeInner 末尾的第二遍。
      if (name === "Table") {
        const raw = doc.sliceString(node.from, node.to);
        const tableStartLine = doc.lineAt(node.from);
        const tableEndLine = doc.lineAt(Math.min(node.to, doc.length));
        const tz: RevealZone = lineZone(tableStartLine.from, tableEndLine.to);
        const tableTouched = touchesLine(sel, tableStartLine.from, tableEndLine.to);
        if (zonedMode) {
          // 恒收录自身 spec；子树留待第二遍（表格还原态）处理
          tableRanges.push({ from: node.from, to: node.to });
          emit({ kind: "table", from: node.from, to: node.to, raw }, [tz]);
          return false;
        }
        if (tableTouched) {
          // 选区触碰表格行 → 还原为源码（继续遍历子节点，行内构造各自判定）
          return;
        }
        tableRanges.push({ from: node.from, to: node.to });
        emit({ kind: "table", from: node.from, to: node.to, raw }, [tz]);
        return false;
      }

      return;
    },
    leave(node) {
      if (!zonedMode) return;
      const name = node.name;
      // 与 enter 中的压栈配对：ATX 压 1 个行区，Setext 压 2 个行区。
      if (ATX_RE.test(name)) {
        negStack.pop();
      } else if (SETEXT_RE.test(name)) {
        negStack.pop();
        negStack.pop();
      }
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
      // 表格区间也跳过（表格 widget 整体替换时不渲染内部 wikilink）。
      // 具体模式走即时判定；zoned 模式记"含该表格即抑制"的 neg 并集。
      const spanTablePos = zonedMode ? tablePosAt(sp.from) : EMPTY_ZONES;
      if (!zonedMode && inRanges(sp.from, tableRanges)) continue;

      // ── `![[...]]` 嵌入语法处理 ──────────────────────────────
      if (sp.embed) {
        // from 向前扩 1 吞掉 `!`，touchesSpan 判定也用扩后区间
        const embFrom = sp.from - 1;
        const embTo = sp.to;
        const embZone: RevealZone = spanZone(embFrom, embTo, 2);

        if (!zonedMode && touchesSpan(sel, embFrom, embTo, 2)) continue;

        const fileName = embedFileName(sp.target);
        const isImage = EMBED_IMAGE_EXT_RE.test(fileName);

        if (isImage) {
          const src = fileName.startsWith("assets/") ? fileName : `assets/${fileName}`;
          // alt 用文件名（去扩展名）：剥掉可能的目录前缀
          const baseName = fileName.includes("/") ? fileName.slice(fileName.lastIndexOf("/") + 1) : fileName;
          const alt = baseName.replace(/\.[^.]+$/, "");

          // embed 图片尺寸语法：sp.label 为 `|` 后段（如 "300" / "300x200"）
          let embWidth: number | undefined;
          let embHeight: number | undefined;
          const embSizeInfo = parseBareSize(sp.label);
          if (embSizeInfo) {
            embWidth = embSizeInfo.width;
            embHeight = embSizeInfo.height;
          }

          // block = 独占段落（仅含 `![[...]]`，无前后文本）
          const isBlock = line.from === embFrom && line.to === embTo;
          if (zonedMode || !touchesSpan(sel, embFrom, embTo, 2)) {
            emit({
              kind: "image",
              from: embFrom,
              to: embTo,
              alt,
              src,
              block: isBlock,
              ...(embWidth !== undefined ? { width: embWidth, height: embHeight } : {}),
            }, [embZone], spanTablePos);
          }
        } else {
          // 非图片嵌入（笔记）：暂退化为 wikilink
          let embLabel = sp.label;
          if (sp.label === sp.target && sp.target.includes("#")) {
            embLabel = sp.target.split("#").join(" > ");
          }
          if (zonedMode || !touchesSpan(sel, embFrom, embTo, 2)) {
            emit({
              kind: "wikilink",
              from: embFrom,
              to: embTo,
              target: sp.target,
              label: embLabel,
            }, [embZone], spanTablePos);
          }
        }
        continue;
      }
      // ── 常规 wikilink ─────────────────────────────────────────

      // 光标/选区进入（含 [[ / ]] 两侧各 2 字符外扩）即还原为源码
      if (!zonedMode && touchesSpan(sel, sp.from, sp.to, 2)) continue;

      // 无别名且 target 含 `#` 时，label 改写为 `a > b > c` 展示路径
      let displayLabel = sp.label;
      if (sp.label === sp.target && sp.target.includes("#")) {
        displayLabel = sp.target.split("#").join(" > ");
      }

      emit({
        kind: "wikilink",
        from: sp.from,
        to: sp.to,
        target: sp.target,
        label: displayLabel,
      }, [spanZone(sp.from, sp.to, 2)], spanTablePos);
    }
  }

  // ── `==highlight==` 行扫描：跳过围栏/行内码区间、表格区间和 frontmatter 区间 ──
  for (let ln = startLineNo; ln <= endLineNo; ln++) {
    const line = doc.line(ln);
    if (fmRange && ln > 0) {
      if (line.from >= fmRange.from && line.from <= fmRange.to) continue;
    }
    // 跳过表格区间内的行（具体模式即时判定；zoned 模式记"含该表格即抑制"的 neg 并集）。
    // 注意：判定用行首 line.from（与旧 `inRanges(line.from, …)` 逐字一致）。
    const hlLineTablePos = zonedMode ? tablePosAt(line.from) : EMPTY_ZONES;
    if (!zonedMode && inRanges(line.from, tableRanges)) continue;
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
      if (!zonedMode && touchesSpan(sel, hl.innerFrom, hl.innerTo, 2)) continue;
      const hlNeg: RevealZone[] = [spanZone(hl.innerFrom, hl.innerTo, 2)];
      // 隐藏两个 `==` 标记
      hide(hl.markFrom, hl.markFrom + 2, hlNeg, hlLineTablePos);
      hide(hl.markTo, hl.markTo + 2, hlNeg, hlLineTablePos);
      // 内容区间加 highlight mark class
      emit({ kind: "highlight", from: hl.innerFrom, to: hl.innerTo, markFrom: hl.markFrom, markTo: hl.markTo }, hlNeg, hlLineTablePos);
    }
  }

  // ── 数学块 `$$...$$` 行扫描 ──────────────────────────────────
  // 逐行扫描，跳过 codeRanges / tableRanges / fmRange，收集 mathBlockLines
  // （行号区间，后续行内 `$...$` 扫描跳过这些行）
  const mathBlockLines: Array<{ start: number; end: number }> = [];
  for (let ln = startLineNo; ln <= endLineNo; ln++) {
    const line = doc.line(ln);
    // 跳过 frontmatter 区间
    if (fmRange && line.from >= fmRange.from && line.from <= fmRange.to) continue;
    // 跳过表格区间（具体模式即时判定；zoned 模式记"含该表格即抑制"的 neg 并集）。
    // 注意：判定用行首 line.from（与旧 `inRanges(line.from, …)` 逐字一致）。
    const mbLineTablePos = zonedMode ? tablePosAt(line.from) : EMPTY_ZONES;
    if (!zonedMode && inRanges(line.from, tableRanges)) continue;
    // 跳过代码区间
    if (inRanges(line.from, codeRanges)) continue;

    const trimmedStart = line.text.trimStart();
    if (!trimmedStart.startsWith("$$")) continue;

    // 计算 `$$` 开标记的绝对位置（跳过前置空白）
    const leadingSpaces = line.text.length - line.text.trimStart().length;
    const openPos = line.from + leadingSpaces;

    // 单行块：`$$...$$` 在同一行闭合
    const afterOpen = trimmedStart.slice(2); // $$ 后的文本
    if (afterOpen.trimEnd().endsWith("$$")) {
      // 单行 `$$ expr $$`：closePos = 行尾 `$$` 之后
      const lineTextTrimmed = line.text.trimEnd();
      const closeOffset = lineTextTrimmed.length;
      const rangeFrom = openPos;
      const rangeTo = line.from + closeOffset;

      // 提取 tex：去掉首尾 $$ 的内容
      const inner = line.text.slice(leadingSpaces + 2, line.from + closeOffset - line.from - 2);
      const tex = inner.trim();

      mathBlockLines.push({ start: ln, end: ln });

      // 光标/选区触碰该块行时还原
      if (!zonedMode && touchesLine(sel, line.from, line.to)) continue;

      emit({ kind: "mathblock", from: rangeFrom, to: rangeTo, tex },
        [lineZone(line.from, line.to)], mbLineTablePos);
      continue;
    }

    // 多行块：向后找闭合 `$$` 行（首行 `$$` 之后到闭合行 `$$` 之前为 tex）
    const blockFrom = openPos;
    // 首行 tex：`$$` 之后到行尾
    const firstLineTex = line.text.slice(leadingSpaces + 2);
    // 中间行缓存（闭合行出现前逐行累积；找到闭合时 texParts = 首行 + 中间行 + 闭合行前段）
    const middleParts: string[] = [];

    for (let inner = ln + 1; inner <= endLineNo; inner++) {
      const innerLine = doc.line(inner);
      // 闭合搜索中止于 frontmatter 边界（未闭合 → 不发射）
      if (fmRange && innerLine.from >= fmRange.from && innerLine.from <= fmRange.to) break;
      const innerTrimmed = innerLine.text.trimEnd();
      if (innerTrimmed.endsWith("$$")) {
        // 闭合行：行首到尾部 `$$` 之前为 tex 末段
        const closeLeadingSpaces = innerLine.text.length - innerLine.text.trimStart().length;
        const closeLen = 2;
        const closePre = innerLine.text.slice(closeLeadingSpaces, innerTrimmed.length - closeLen);
        const tex = [firstLineTex, ...middleParts, closePre].join("\n").trim();

        const rangeTo = innerLine.from + innerTrimmed.length;
        mathBlockLines.push({ start: ln, end: inner });

        // 光标/选区触碰该块任一行时还原（zoned 模式：块内逐行记 neg 区）
        const blockStartLine = doc.lineAt(blockFrom).number;
        if (zonedMode) {
          const blockZones: RevealZone[] = [];
          for (let bl = blockStartLine; bl <= inner; bl++) {
            const blLine = doc.line(bl);
            blockZones.push(lineZone(blLine.from, blLine.to));
          }
          emit({ kind: "mathblock", from: blockFrom, to: rangeTo, tex }, [...blockZones], mbLineTablePos);
        } else {
          let touched = false;
          for (let bl = blockStartLine; bl <= inner; bl++) {
            const blLine = doc.line(bl);
            if (touchesLine(sel, blLine.from, blLine.to)) { touched = true; break; }
          }
          if (!touched) {
            emit({ kind: "mathblock", from: blockFrom, to: rangeTo, tex }, EMPTY_ZONES);
          }
        }

        ln = inner; // 跳过已处理的行
        break;
      }
      middleParts.push(innerLine.text);
    }
    // 未找到闭合行：不加入 mathBlockRanges，不发射 spec
  }

  // ── 行内数学 `$...$` 行扫描：跳过 code / table / fm / mathBlock 区间 ──
  for (let ln = startLineNo; ln <= endLineNo; ln++) {
    const line = doc.line(ln);
    // 跳过 frontmatter 区间
    if (fmRange && line.from >= fmRange.from && line.from <= fmRange.to) continue;
    // 跳过表格区间（具体模式即时判定；zoned 模式记"含该表格即抑制"的 neg 并集）
    const miLineTablePos = zonedMode ? tablePosAt(line.from) : EMPTY_ZONES;
    if (!zonedMode && inRanges(line.from, tableRanges)) continue;
    // 跳过代码区间
    if (inRanges(line.from, codeRanges)) continue;
    // 跳过块级数学区间内的行
    let inMathBlock = false;
    for (const r of mathBlockLines) {
      if (ln >= r.start && ln <= r.end) { inMathBlock = true; break; }
    }
    if (inMathBlock) continue;

    const mathMatches = scanInlineMathInLine(line.text, line.from);
    for (const m of mathMatches) {
      if (m.to <= lo || m.from >= hi) continue;
      // 还原：光标触碰 `$` 符号本身（pad=1）时不渲染
      if (!zonedMode && touchesSpan(sel, m.from, m.to, 1)) continue;
      emit({ kind: "mathinline", from: m.from, to: m.to, tex: m.tex },
        [spanZone(m.from, m.to, 1)], miLineTablePos);
    }
  }

  // ── frontmatter 折叠：选区不触碰时发射 spec ──────────────────────
  // zoned 模式恒发射（fmCollapsed 的判定区显式记录）；具体模式保持旧逻辑。
  if (fmRange && (zonedMode || fmCollapsed)) {
    const firstLine = doc.lineAt(fmRange.from);
    const lastLine = doc.lineAt(fmRange.to);
    // 仅在可见区间内才发射
    if (fmRange.from < hi && fmRange.to > lo) {
      const propCount = Math.max(0, lastLine.number - firstLine.number - 1);
      emit({
        kind: "frontmatter",
        from: fmRange.from,
        to: fmRange.to,
        propCount,
      }, zonedMode ? [lineZone(firstLine.from, lastLine.to)] : EMPTY_ZONES);
    }
  }

  // ── zoned 第二遍：表格还原态子树 ──────────────────────────────────
  // 具体计算中表格还原（选区触碰表格）时会进入子树，子树内的 Image/Link/
  // Autolink/行内强调按自身守卫发射（此时表格抑制判定恒为假），且发射顺序
  // 位于表格 spec 之前（先序遍历：子树产物早于表格的 `return false` 结束点；
  // 行扫描的 wikilink/highlight/math 系则位于整树遍历之后）。
  // zoned 下 sel 为空，第一遍恒不进入子树；第二遍对每张表格单独重走其子树
  // 收集产物（记 pos 区 = 该表格判定区 + 自身 neg 守卫），再把每块产物搬到
  // 对应表格 spec 之前，使整体顺序与具体计算逐字一致。
  // 行扫描不需第二遍（扫描天然逐行经过表格行，pos 区已在第一遍记录）。
  if (zonedMode) {
    for (const t of tableRanges) {
      const collected: ZonedSpec[] = [];
      collectRevealedTableSubtree(state, doc, t, collected);
      if (collected.length > 0) {
        const idx = zoned.findIndex(
          (z) => z.spec.kind === "table" && z.spec.from === t.from && z.spec.to === t.to,
        );
        if (idx >= 0) {
          zoned.splice(idx, 0, ...collected);
        } else {
          zoned.push(...collected);
        }
      }
    }
  }

  return { specs, zoned: zonedMode ? zoned : null };
}

/**
 * zoned 第二遍：重走单个表格的子树，收集"表格还原态"下子树内 Image/Link/
 * Autolink 发射的 spec（附 pos 区 = 表格判定区 + 自身 neg 守卫）。
 * 表格未还原时子树不被访问（第一遍 `return false`），因此此处只处理还原态。
 */
function collectRevealedTableSubtree(
  state: EditorState,
  doc: EditorState["doc"],
  table: { from: number; to: number },
  zoned: ZonedSpec[],
): void {
  const tree = syntaxTree(state);
  const tableZone: RevealZone = (() => {
    const startLine = doc.lineAt(table.from);
    const endLine = doc.lineAt(Math.min(table.to, doc.length));
    return { from: startLine.from, to: endLine.to };
  })();
  const sel: readonly SelRange[] = EMPTY_RANGES;
  tree.iterate({
    from: table.from,
    to: table.to,
    enter(node) {
      // 表格还原态下进入子树：Strong/Emphasis/Strikethrough/InlineCode 的
      // 标记 hide、Image/Link/Autolink 各自判定（表格抑制恒为假）。
      // 收集顺序与第一遍一致（文档顺序），保证过滤后顺序与具体计算一致。
      if (
        node.name === "StrongEmphasis"
        || node.name === "Emphasis"
        || node.name === "Strikethrough"
        || node.name === "InlineCode"
      ) {
        collectTableEmphasis(doc, node, tableZone, zoned);
        return;
      }
      if (node.name === "Image") {
        collectTableImage(doc, node, tableZone, sel, zoned);
        return false;
      }
      if (node.name === "Link") {
        collectTableLink(doc, node, tableZone, sel, zoned);
        return false;
      }
      if (node.name === "Autolink") {
        collectTableAutolink(doc, node, tableZone, sel, zoned);
        return false;
      }
      return;
    },
  });
}

/**
 * 表格还原态子树内的行内强调：标记 hide，自身 span 守卫记 neg、表格记 pos。
 * （标题抑制不适用：标题内不可能出现表格。）
 */
function collectTableEmphasis(
  _doc: EditorState["doc"],
  node: SyntaxNodeRef,
  tableZone: RevealZone,
  zoned: ZonedSpec[],
): void {
  const marks = node.node
    .getChildren("EmphasisMark")
    .concat(
      node.node.getChildren("StrikethroughMark"),
      node.node.getChildren("CodeMark"),
    );
  if (marks.length === 0) return;
  const pad = marks[0].to - marks[0].from;
  const neg: RevealZone[] = [{ from: node.from - pad, to: node.to + pad }];
  for (const mk of marks) {
    if (mk.to > mk.from) {
      zoned.push({
        spec: { kind: "hide", from: mk.from, to: mk.to },
        neg,
        pos: [tableZone],
      });
    }
  }
}

/** 表格还原态子树内的 Image：自身守卫（block 用行 / inline 用 span+1）。 */
function collectTableImage(
  doc: EditorState["doc"],
  node: SyntaxNodeRef,
  tableZone: RevealZone,
  _sel: readonly SelRange[],
  zoned: ZonedSpec[],
): void {
  // 抑制 `![[...]]` 嵌入语法（与第一遍一致，由行扫描统一处理）
  if (node.from < node.to) {
    const raw = doc.sliceString(node.from, node.to);
    if (raw.startsWith("![[") && raw.endsWith("]]") && raw.length >= 6) return;
  }
  const urlNode = node.node.getChild("URL");
  const rawSrc = urlNode ? doc.sliceString(urlNode.from, urlNode.to) : "";
  const src = rawSrc.startsWith("<") && rawSrc.endsWith(">")
    ? rawSrc.slice(1, -1)
    : rawSrc;
  const linkMarks = node.node.getChildren("LinkMark");
  let alt = linkMarks.length >= 2
    ? doc.sliceString(linkMarks[0].to, linkMarks[1].from)
    : "";
  let imgWidth: number | undefined;
  let imgHeight: number | undefined;
  const sizeInfo = splitImageSize(alt);
  if (sizeInfo) {
    imgWidth = sizeInfo.width;
    imgHeight = sizeInfo.height;
    alt = sizeInfo.rest.trimEnd();
  }
  const isAttachment = isAttachmentSrc(src);
  const resolvedSrc = isAttachment ? normalizeAttachmentSrc(src) : src;
  const parent = node.node.parent;
  const isBlock = parent?.name === "Paragraph"
    && parent.from === node.from
    && parent.to === node.to;
  const line = doc.lineAt(node.from);
  // 还原态下恒发射（sel 为空），记自身守卫为 neg、表格为 pos
  const neg: RevealZone[] = isBlock
    ? [{ from: line.from, to: line.to }]
    : [{ from: node.from - 1, to: node.to + 1 }];
  zoned.push({
    spec: {
      kind: "image",
      from: node.from,
      to: node.to,
      alt,
      src: resolvedSrc,
      block: isBlock,
      ...(imgWidth !== undefined ? { width: imgWidth, height: imgHeight } : {}),
    },
    neg,
    pos: [tableZone],
  });
}

/** 表格还原态子树内的 Link（含引用式）：自身行守卫 + 表格 pos 区。 */
function collectTableLink(
  doc: EditorState["doc"],
  node: SyntaxNodeRef,
  tableZone: RevealZone,
  _sel: readonly SelRange[],
  zoned: ZonedSpec[],
): void {
  const urlNode = node.node.getChild("URL");
  const line = doc.lineAt(node.from);
  const neg: RevealZone[] = [{ from: line.from, to: line.to }];
  const linkMarks = node.node.getChildren("LinkMark");
  const text = linkMarks.length >= 2
    ? doc.sliceString(linkMarks[0].to, linkMarks[1].from)
    : "";
  if (!urlNode) {
    if (!text) return;
    // 引用式：查定义表（与第一遍一致的惰性全表扫描）
    const refLabelNode = node.node.getChild("LinkLabel");
    let refKey = text;
    if (refLabelNode) {
      const rawRef = doc.sliceString(refLabelNode.from, refLabelNode.to);
      const stripped = rawRef.startsWith("[") && rawRef.endsWith("]")
        ? rawRef.slice(1, -1)
        : rawRef;
      if (stripped.trim()) refKey = stripped;
    }
    const url = buildRefDefs(doc).get(refKey.toLowerCase().trim());
    if (!url) return;
    zoned.push({
      spec: { kind: "mdlink", from: node.from, to: node.to, text, url },
      neg,
      pos: [tableZone],
    });
    return;
  }
  const url = doc.sliceString(urlNode.from, urlNode.to);
  zoned.push({
    spec: { kind: "mdlink", from: node.from, to: node.to, text, url },
    neg,
    pos: [tableZone],
  });
}

/** 表格还原态子树内的 Autolink：尖括号 hide，自身行守卫 + 表格 pos 区。 */
function collectTableAutolink(
  doc: EditorState["doc"],
  node: SyntaxNodeRef,
  tableZone: RevealZone,
  _sel: readonly SelRange[],
  zoned: ZonedSpec[],
): void {
  const line = doc.lineAt(node.from);
  const neg: RevealZone[] = [{ from: line.from, to: line.to }];
  for (const lm of node.node.getChildren("LinkMark")) {
    if (lm.to > lm.from) {
      zoned.push({
        spec: { kind: "hide", from: lm.from, to: lm.to },
        neg,
        pos: [tableZone],
      });
    }
  }
}
