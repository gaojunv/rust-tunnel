import { Facet } from "@codemirror/state";
import { EditorView, WidgetType } from "@codemirror/view";

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
 * 风格参考 GitHub markdown，颜色走 CSS 变量自适应深浅色。
 */
export class TableWidget extends WidgetType {
  constructor(readonly raw: string) {
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

    const table = document.createElement("table");
    table.className = "cm-lp-table";

    // thead
    const thead = document.createElement("thead");
    const headTr = document.createElement("tr");
    headerCells.forEach((cell, i) => {
      const th = document.createElement("th");
      th.textContent = cell;
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
        td.textContent = cell;
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
