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
