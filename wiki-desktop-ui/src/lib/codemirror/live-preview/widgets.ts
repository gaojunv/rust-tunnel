import { Facet } from "@codemirror/state";
import { EditorView, WidgetType } from "@codemirror/view";
import katex from "katex";
import "katex/dist/katex.min.css";

/**
 * Live Preview 视图 widget 层。
 * 纯展示/交互，不直接读语法树——消费 `spec.ts` 输出的结构化描述。
 * view 与区间经构造函数闭包传入（index.ts 在 `decorations(view)` 回调中创建）。
 */

/**
 * 导航回调 facet：WikilinkWidget 点击 → (targetKey: string)。
 * MarkdownEditor 以 ref 转发最新 handler。
 */
export const wikilinkNavFacet = Facet.define<(key: string) => void>();

const CHECK_EMPTY = "[ ]";
const CHECK_FULL = "[x]";

/**
 * 任务复选框。点击在 `[ ]` / `[x]` 之间翻转，直接在原文上改动（Decoration 是
 * 纯视图层，format-commands/搜索/补全都仍作用于原文）。
 */
export class CheckboxWidget extends WidgetType {
  constructor(
    readonly checked: boolean,
    private readonly view: EditorView,
    private readonly from: number,
    private readonly to: number,
  ) {
    super();
  }

  eq(other: WidgetType): boolean {
    return (
      other instanceof CheckboxWidget &&
      other.checked === this.checked &&
      other.from === this.from &&
      other.to === this.to
    );
  }

  toDOM(): HTMLElement {
    const box = document.createElement("input");
    box.type = "checkbox";
    box.className = "cm-lp-checkbox-input";
    box.checked = this.checked;
    box.tabIndex = -1;
    box.addEventListener("mousedown", (e) => {
      e.preventDefault();
      e.stopPropagation();
    });
    box.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
      const insert = this.checked ? CHECK_EMPTY : CHECK_FULL;
      this.view.dispatch({ changes: { from: this.from, to: this.to, insert } });
      this.view.focus();
    });
    return box;
  }

  ignoreEvent(event: Event): boolean {
    // 指针事件完全自管，避免 CM 把光标落入被替换的区间
    const t = event.type;
    return t === "mousedown" || t === "click";
  }
}

/**
 * wikilink 链接式 widget。整段 `[[target|label]]` 替换为可点击的链接样式；
 * 点击经 `wikilinkNavFacet` 回调导航。
 */
export class WikilinkWidget extends WidgetType {
  constructor(
    readonly target: string,
    readonly label: string,
    private readonly view: EditorView,
  ) {
    super();
  }

  eq(other: WidgetType): boolean {
    return (
      other instanceof WikilinkWidget &&
      other.target === this.target &&
      other.label === this.label
    );
  }

  toDOM(): HTMLElement {
    const el = document.createElement("span");
    el.className = "cm-lp-wikilink";
    el.textContent = this.label;
    el.title = this.target;
    el.addEventListener("mousedown", (e) => e.preventDefault());
    el.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
      const handler = this.view.state.facet(wikilinkNavFacet);
      const fn = handler.length > 0 ? handler[handler.length - 1] : undefined;
      fn?.(this.target);
    });
    return el;
  }

  ignoreEvent(): boolean {
    return false;
  }
}

/**
 * 分隔线（`---`）替换为整行横线。
 */
export class HrWidget extends WidgetType {
  eq(other: WidgetType): boolean {
    return other instanceof HrWidget;
  }

  toDOM(): HTMLElement {
    const el = document.createElement("div");
    el.className = "cm-lp-hr";
    return el;
  }

  ignoreEvent(): boolean {
    return true;
  }
}

// ── 图片 widget ──────────────────────────────────────────────────────────────

/**
 * 图片渲染 widget。附件路径通过动态 import 异步解析为 blob URL；
 * 普通 http(s)/data: src 直接渲染 `<img>`。
 *
 * 点击图片将光标移入源码区间，自然还原为 markdown 源码。
 */
/** 附件异步加载的取消句柄（挂在 widget DOM 上，destroy 时调用）。 */
type CancelableDom = HTMLElement & { __cancel?: () => void };

export class ImageWidget extends WidgetType {
  constructor(
    readonly alt: string,
    readonly src: string,
    readonly isAttachment: boolean,
    private readonly view: EditorView,
    private readonly from: number,
    readonly block: boolean,
  ) {
    super();
  }

  eq(other: WidgetType): boolean {
    return (
      other instanceof ImageWidget &&
      other.src === this.src &&
      other.alt === this.alt &&
      other.block === this.block
    );
  }

  toDOM(): HTMLElement {
    const wrap = document.createElement("span");
    wrap.className = "cm-lp-image-wrap";
    if (this.block) wrap.classList.add("cm-lp-image-block");

    // 占位 span（加载中 / 失败时显示）
    const placeholder = document.createElement("span");
    placeholder.className = "cm-lp-image-ph";
    placeholder.textContent = this.alt || "image";

    if (this.isAttachment) {
      // 附件路径：异步加载为 blob URL
      const img = document.createElement("img");
      img.className = "cm-lp-image";
      img.alt = this.alt;
      img.style.maxWidth = "100%";
      img.style.borderRadius = "6px";
      img.style.cursor = "pointer";

      // 先显示 placeholder，加载完成后替换
      wrap.appendChild(placeholder);
      let cancelled = false;

      // 异步加载附件 blob URL（单飞 + LRU 缓存，由 attachments.ts 管理）
      void (async () => {
        try {
          const mod = await import("@/lib/attachments");
          const blobUrl = await mod.getAttachmentUrl(this.src);
          if (cancelled) return;
          img.src = blobUrl;
          img.onerror = () => {
            if (!cancelled) {
              placeholder.textContent = this.alt || "broken";
            }
          };
          if (!cancelled) {
            placeholder.replaceWith(img);
          }
        } catch {
          if (!cancelled) {
            placeholder.textContent = this.alt || "error";
          }
        }
      })();

      // 暴露取消句柄给 destroy()
      (wrap as CancelableDom).__cancel = () => {
        cancelled = true;
      };
    } else {
      // 普通 URL：直接渲染
      const img = document.createElement("img");
      img.className = "cm-lp-image";
      img.src = this.src;
      img.alt = this.alt;
      img.style.maxWidth = "100%";
      img.style.borderRadius = "6px";
      img.style.cursor = "pointer";
      img.onerror = () => {
        img.replaceWith(placeholder);
        placeholder.textContent = this.alt || "broken";
      };
      wrap.appendChild(img);
    }

    // 点击 → 光标移入源码区间，自然还原
    wrap.style.cursor = "pointer";
    wrap.addEventListener("click", (e) => {
      e.preventDefault();
      this.view.dispatch({ selection: { anchor: this.from } });
      this.view.focus();
    });

    return wrap;
  }

  destroy(dom: HTMLElement): void {
    (dom as CancelableDom).__cancel?.();
  }

  ignoreEvent(): boolean {
    return false;
  }
}

// ── Markdown 链接 widget ─────────────────────────────────────────────────────

/**
 * Markdown 链接 `[text](url)` 替换为带样式的链接文本。
 * 点击将光标移入源码区间；Mod+Click 由 index.ts 的 domEventHandlers 处理打开外部链接。
 */
export class MdLinkWidget extends WidgetType {
  constructor(
    readonly text: string,
    readonly url: string,
    private readonly view: EditorView,
  ) {
    super();
  }

  eq(other: WidgetType): boolean {
    return (
      other instanceof MdLinkWidget &&
      other.text === this.text &&
      other.url === this.url
    );
  }

  toDOM(): HTMLElement {
    const el = document.createElement("span");
    el.className = "cm-lp-mdlink";
    el.textContent = this.text;
    el.title = this.url;
    el.addEventListener("mousedown", (e) => e.preventDefault());
    el.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
      // Mod+Click（Cmd/Ctrl）：打开外部链接
      if (e.metaKey || e.ctrlKey) {
        try {
          window.open(this.url, "_blank");
        } catch {
          // ignore — invalid URL
        }
        return;
      }
      // 普通点击：还原为源码（光标移入）
      this.view.dispatch({
        selection: { anchor: this.view.posAtCoords({ x: e.clientX, y: e.clientY }, false) ?? 0 },
      });
      this.view.focus();
    });
    return el;
  }

  ignoreEvent(): boolean {
    return false;
  }
}

// ── 表格单元格内联渲染器 ─────────────────────────────────────────────────────

type InlineHandlers = {
  onLink: (url: string) => void;
  onWikilink: (key: string) => void;
};

/**
 * 纯 DOM 单元格内联渲染器（不依赖 lezer / @codemirror/view）。
 * 解析 `**bold**` / `*em*` / `~~del~~` / `` `code` `` / `[text](url)` /
 * `[[target|label]]` / `==highlight==` / `![alt](src)` 为 DocumentFragment。
 * XSS 安全：一律用 textContent / createTextNode，禁止 innerHTML。
 *
 * 解析优先级：代码 > 链接/wikilink > 标记符号 > 高亮 > 文本。
 * 嵌套处理：链接/wikilink 文本内支持粗/斜（递归）；代码内不做任何解析。
 */
export function renderTableCellInline(
  text: string,
  handlers: InlineHandlers,
): DocumentFragment {
  const frag = document.createDocumentFragment();
  let pos = 0;

  function flushText(from: number, to: number): void {
    if (to > from) frag.appendChild(document.createTextNode(text.slice(from, to)));
  }

  while (pos < text.length) {
    // 1) 行内代码（最高优先级）
    if (text[pos] === "`") {
      let openLen = 0;
      while (pos + openLen < text.length && text[pos + openLen] === "`") openLen++;
      const openEnd = pos + openLen;
      const closeIdx = text.indexOf("`".repeat(openLen), openEnd);
      if (closeIdx !== -1) {
        const codeEl = document.createElement("code");
        codeEl.className = "cm-lp-code";
        codeEl.textContent = text.slice(openEnd, closeIdx);
        frag.appendChild(codeEl);
        pos = closeIdx + openLen;
        continue;
      }
      // 未闭合：当作普通文本输出反引号
      flushText(pos, pos + 1);
      pos++;
      continue;
    }

    // 2) Markdown 链接 [text](url)（LinkMark 不在纯文本中出现）
    if (text[pos] === "[" && text[pos + 1] !== "[") {
      const bracketClose = text.indexOf("]", pos + 2);
      if (bracketClose !== -1 && text[bracketClose + 1] === "(") {
        const parenClose = text.indexOf(")", bracketClose + 2);
        if (parenClose !== -1) {
          const linkText = text.slice(pos + 1, bracketClose);
          const url = text.slice(bracketClose + 2, parenClose);
          if (url && !url.startsWith("!")) {
            const span = document.createElement("span");
            span.className = "cm-lp-mdlink";
            span.textContent = linkText;
            span.title = url;
            span.addEventListener("mousedown", (e) => e.preventDefault());
            span.addEventListener("click", (e) => {
              e.preventDefault();
              e.stopPropagation();
              try { window.open(url, "_blank"); } catch { /* invalid URL */ }
            });
            frag.appendChild(span);
            pos = parenClose + 1;
            continue;
          }
        }
      }
    }

    // 3) Wikilink [[target|label]]
    if (text[pos] === "[" && text[pos + 1] === "[") {
      const closeIdx = text.indexOf("]]", pos + 2);
      if (closeIdx !== -1) {
        const inner = text.slice(pos + 2, closeIdx);
        if (inner.trim()) {
          const bar = inner.indexOf("|");
          const target = (bar === -1 ? inner : inner.slice(0, bar)).trim();
          const label = (bar === -1 ? inner : inner.slice(bar + 1)).trim() || target;
          if (target) {
            const span = document.createElement("span");
            span.className = "cm-lp-wikilink";
            span.textContent = label;
            span.title = target;
            span.addEventListener("mousedown", (e) => e.preventDefault());
            span.addEventListener("click", (e) => {
              e.preventDefault();
              e.stopPropagation();
              handlers.onWikilink(target);
            });
            frag.appendChild(span);
            pos = closeIdx + 2;
            continue;
          }
        }
      }
    }

    // 4) ==highlight== （`==` 两侧不粘连 `=`）
    if (
      text[pos] === "=" && text[pos + 1] === "="
      && (pos === 0 || text[pos - 1] !== "=")
    ) {
      const closeIdx = text.indexOf("==", pos + 2);
      if (
        closeIdx !== -1
        && (closeIdx + 2 >= text.length || text[closeIdx + 2] !== "=")
      ) {
        const innerLen = closeIdx - (pos + 2);
        if (innerLen > 0) {
          const span = document.createElement("span");
          span.className = "cm-lp-highlight";
          span.textContent = text.slice(pos + 2, closeIdx);
          frag.appendChild(span);
          pos = closeIdx + 2;
          continue;
        }
      }
    }

    // 5) 粗体 **text**（必须在 *text* 之前匹配）
    if (text[pos] === "*" && text[pos + 1] === "*") {
      const closeIdx = text.indexOf("**", pos + 2);
      if (closeIdx !== -1 && closeIdx - pos > 2) {
        const strong = document.createElement("strong");
        strong.textContent = text.slice(pos + 2, closeIdx);
        frag.appendChild(strong);
        pos = closeIdx + 2;
        continue;
      }
    }

    // 6) 斜体 *text*
    if (text[pos] === "*") {
      const closeIdx = text.indexOf("*", pos + 1);
      if (closeIdx !== -1 && closeIdx - pos > 1) {
        const em = document.createElement("em");
        em.textContent = text.slice(pos + 1, closeIdx);
        frag.appendChild(em);
        pos = closeIdx + 1;
        continue;
      }
    }

    // 7) 删除线 ~~text~~
    if (text[pos] === "~" && text[pos + 1] === "~") {
      const closeIdx = text.indexOf("~~", pos + 2);
      if (closeIdx !== -1) {
        const del = document.createElement("s");
        del.textContent = text.slice(pos + 2, closeIdx);
        frag.appendChild(del);
        pos = closeIdx + 2;
        continue;
      }
    }

    // 8) 图片 ![alt](src) —— 降级为纯文本 alt
    if (text[pos] === "!" && text[pos + 1] === "[") {
      const bracketClose = text.indexOf("]", pos + 2);
      if (bracketClose !== -1 && text[bracketClose + 1] === "(") {
        const parenClose = text.indexOf(")", bracketClose + 2);
        if (parenClose !== -1) {
          const alt = text.slice(pos + 2, bracketClose);
          frag.appendChild(document.createTextNode(alt));
          pos = parenClose + 1;
          continue;
        }
      }
    }

    // 9) 普通文本：吞到下一个特殊字符
    let end = pos + 1;
    while (end < text.length && !"*~`[!=\\".includes(text[end])) end++;
    flushText(pos, end);
    pos = end;
  }

  return frag;
}

// ── 表格 widget ──────────────────────────────────────────────────────────────

function splitTableRow(line: string): string[] {
  const trimmed = line.trimStart();
  let content = trimmed;
  if (content.startsWith("|")) content = content.slice(1);
  if (content.endsWith("|")) content = content.slice(0, -1);
  return content.split("|").map((c) => c.trim());
}

function isDelimiterRow(cells: string[]): boolean {
  return cells.length > 0 && cells.every((c) => /^:?-{2,}:?$/.test(c));
}

function parseTableAlign(cells: string[]): Array<"left" | "center" | "right"> {
  return cells.map((c) => {
    const left = c.startsWith(":");
    const right = c.endsWith(":");
    if (left && right) return "center" as const;
    if (right) return "right" as const;
    return "left" as const;
  });
}

/**
 * GFM 表格渲染为 HTML `<table>`。
 * 单元格内联格式通过 `renderTableCellInline` 渲染（粗体/行内码/链接/wikilink 等）。
 */
export class TableWidget extends WidgetType {
  constructor(
    readonly raw: string,
    private readonly view: EditorView,
  ) {
    super();
  }

  eq(other: WidgetType): boolean {
    return other instanceof TableWidget && other.raw === this.raw;
  }

  toDOM(): HTMLTableElement {
    const lines = this.raw.split("\n").filter((l) => l.trim());
    if (lines.length === 0) {
      const t = document.createElement("table");
      t.className = "cm-lp-table";
      return t;
    }

    // 解析表头
    const headerCells = splitTableRow(lines[0]);
    // 寻找分隔行（含 `---`）
    let aligns: Array<"left" | "center" | "right"> = [];
    let bodyStart = 1;
    for (let i = 1; i < lines.length; i++) {
      const cells = splitTableRow(lines[i]);
      if (isDelimiterRow(cells)) {
        aligns = parseTableAlign(cells);
        bodyStart = i + 1;
        break;
      }
    }

    const navHandler = this.view.state.facet(wikilinkNavFacet);
    const nav = navHandler.length > 0 ? navHandler[navHandler.length - 1] : undefined;
    const handlers: InlineHandlers = {
      onLink: (url) => { try { window.open(url, "_blank"); } catch { /* ignore */ } },
      onWikilink: (key) => { nav?.(key); },
    };

    const table = document.createElement("table");
    table.className = "cm-lp-table";

    // thead
    const thead = document.createElement("thead");
    const headTr = document.createElement("tr");
    headerCells.forEach((cell, i) => {
      const th = document.createElement("th");
      th.appendChild(renderTableCellInline(cell, handlers));
      if (aligns[i]) th.style.textAlign = aligns[i];
      headTr.appendChild(th);
    });
    thead.appendChild(headTr);
    table.appendChild(thead);

    // tbody
    const tbody = document.createElement("tbody");
    for (let i = bodyStart; i < lines.length; i++) {
      const cells = splitTableRow(lines[i]);
      if (isDelimiterRow(cells)) continue;
      const tr = document.createElement("tr");
      cells.forEach((cell, j) => {
        const td = document.createElement("td");
        td.appendChild(renderTableCellInline(cell, handlers));
        if (aligns[j]) td.style.textAlign = aligns[j];
        tr.appendChild(td);
      });
      tbody.appendChild(tr);
    }
    if (tbody.children.length > 0) table.appendChild(tbody);

    return table;
  }

  ignoreEvent(): boolean {
    return true;
  }
}

// ── 代码块围栏行 widget ──────────────────────────────────────────────────────

/**
 * 代码块开围栏行（``` + 语言标签）替换为细条 widget。
 * 左侧显示语言名（小字 muted），样式类 `cm-lp-code-header`。
 */
export class CodeHeaderWidget extends WidgetType {
  constructor(readonly lang: string) {
    super();
  }

  eq(other: WidgetType): boolean {
    return other instanceof CodeHeaderWidget && other.lang === this.lang;
  }

  toDOM(): HTMLElement {
    const el = document.createElement("div");
    el.className = "cm-lp-code-header";
    if (this.lang) {
      const tag = document.createElement("span");
      tag.className = "cm-lp-code-lang";
      tag.textContent = this.lang;
      el.appendChild(tag);
    }
    return el;
  }

  ignoreEvent(): boolean {
    return true;
  }
}

/**
 * 代码块闭围栏行（```）替换为零高度 widget。
 */
export class CodeFooterWidget extends WidgetType {
  eq(other: WidgetType): boolean {
    return other instanceof CodeFooterWidget;
  }

  toDOM(): HTMLElement {
    const el = document.createElement("div");
    el.className = "cm-lp-code-footer";
    return el;
  }

  ignoreEvent(): boolean {
    return true;
  }
}

// ── Frontmatter 折叠 widget ───────────────────────────────────────────────────

/**
 * YAML frontmatter 折叠为单行 muted 细条。
 * 显示 `··· N properties ···`，点击将光标移入区间起点还原为源码。
 */
export class FrontmatterWidget extends WidgetType {
  constructor(
    readonly propCount: number,
    private readonly view: EditorView,
    private readonly from: number,
  ) {
    super();
  }

  eq(other: WidgetType): boolean {
    return (
      other instanceof FrontmatterWidget &&
      other.propCount === this.propCount
    );
  }

  toDOM(): HTMLElement {
    const el = document.createElement("div");
    el.className = "cm-lp-frontmatter";
    const label = this.propCount === 1 ? "1 property" : `${this.propCount} properties`;
    el.textContent = `··· ${label} ···`;
    el.addEventListener("click", (e) => {
      e.preventDefault();
      // 光标移入 frontmatter 区间起点 → 还原为源码
      this.view.dispatch({ selection: { anchor: this.from } });
      this.view.focus();
    });
    return el;
  }

  ignoreEvent(): boolean {
    return false;
  }
}

// ── Callout 标签 widget ────────────────────────────────────────────────────

/** callout 类型 → 图标（文本符号） */
const CALLOUT_ICONS: Record<string, string> = {
  note: "📝",
  tip: "💡",
  success: "✅",
  warning: "⚠️",
  caution: "⚠️",
  important: "❗",
  question: "❓",
  danger: "🔥",
  error: "🔥",
  failure: "🔥",
  info: "ℹ️",
};

/** 首字母大写：类型显示名 */
function capitalizeCalloutType(t: string): string {
  return t.charAt(0).toUpperCase() + t.slice(1);
}

/**
 * Callout `[!type]` 标记替换为带图标的标签条。
 * 显示图标 + title（同行标题文字）或类型显示名。
 */
export class CalloutWidget extends WidgetType {
  constructor(
    readonly calloutType: string,
    readonly title: string,
  ) {
    super();
  }

  eq(other: WidgetType): boolean {
    return (
      other instanceof CalloutWidget &&
      other.calloutType === this.calloutType &&
      other.title === this.title
    );
  }

  toDOM(): HTMLElement {
    const el = document.createElement("span");
    el.className = "cm-lp-callout-label";
    const icon = document.createElement("span");
    icon.className = "cm-lp-callout-icon";
    icon.textContent = CALLOUT_ICONS[this.calloutType] ?? "📌";
    el.appendChild(icon);
    const label = document.createElement("span");
    label.textContent = this.title || capitalizeCalloutType(this.calloutType);
    el.appendChild(label);
    return el;
  }

  ignoreEvent(): boolean {
    return true;
  }
}

// ── 数学公式 widget ────────────────────────────────────────────────────────

/**
 * KaTeX 数学公式渲染。`$$...$$` 块级公式（displayMode）与 `$...$` 行内公式共用。
 * 渲染失败时退化为等宽斜体原文显示（`cm-lp-math-error`）。
 *
 * 点击将光标移入源码区间，自然还原为公式源码。
 */
export class MathWidget extends WidgetType {
  constructor(
    readonly tex: string,
    readonly displayMode: boolean,
    private readonly view: EditorView,
    private readonly from: number,
  ) {
    super();
  }

  eq(other: WidgetType): boolean {
    return (
      other instanceof MathWidget &&
      other.tex === this.tex &&
      other.displayMode === this.displayMode
    );
  }

  toDOM(): HTMLElement {
    const el = document.createElement("div");
    el.className = this.displayMode ? "cm-lp-math cm-lp-math-block" : "cm-lp-math";
    try {
      el.innerHTML = katex.renderToString(this.tex, {
        displayMode: this.displayMode,
        throwOnError: false,
      });
    } catch {
      el.textContent = this.tex;
      el.classList.add("cm-lp-math-error");
    }
    el.style.cursor = "pointer";
    el.addEventListener("click", (e) => {
      e.preventDefault();
      this.view.dispatch({ selection: { anchor: this.from } });
      this.view.focus();
    });
    return el;
  }

  ignoreEvent(): boolean {
    return false;
  }
}

// ── 无序列表圆点 widget ────────────────────────────────────────────────────

/**
 * 无序列表标记（`-` / `+` / `*`）替换为统一样式的圆点。
 * 纯展示，无交互（点击穿透由编辑器处理）。
 */
export class BulletWidget extends WidgetType {
  eq(other: WidgetType): boolean {
    return other instanceof BulletWidget;
  }

  toDOM(): HTMLElement {
    const el = document.createElement("span");
    el.className = "cm-lp-bullet";
    el.textContent = "•";
    return el;
  }

  ignoreEvent(): boolean {
    return true;
  }
}
