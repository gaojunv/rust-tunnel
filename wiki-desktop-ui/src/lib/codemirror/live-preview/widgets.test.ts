/**
 * @vitest-environment jsdom
 *
 * widgets 层测试（DOM 渲染）：
 * - `renderTableCellInline` 纯 DOM 内联渲染器（粗/斜/删/行内码/链接/wikilink/高亮/图片降级/XSS）
 * - `CalloutWidget` DOM 结构
 * - `TableWidget` 单元格内联渲染集成（含 view 构造参数）
 */
import { describe, it, expect, vi } from "vitest";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { livePreview } from "./index";
import {
  BulletWidget,
  CalloutWidget,
  CheckboxWidget,
  FrontmatterWidget,
  ImageWidget,
  MathWidget,
  clearKatexCacheForTest,
  katexCacheSizeForTest,
  renderTableCellInline,
  revealAtClick,
  TableWidget,
  widgetRangeOf,
  wikilinkNavFacet,
} from "./widgets";

function makeView(): EditorView {
  return new EditorView({
    state: EditorState.create({
      doc: "",
      extensions: [
        markdown({ base: markdownLanguage }),
        wikilinkNavFacet.of(() => {}),
      ],
    }),
  });
}

function renderToHtml(text: string): string {
  const frag = renderTableCellInline(text, { onLink: () => {}, onWikilink: () => {} });
  const host = document.createElement("div");
  host.appendChild(frag);
  return host.innerHTML;
}

describe("renderTableCellInline", () => {
  it("**bold** → <strong>", () => {
    const html = renderToHtml("a **b** c");
    expect(html).toContain("<strong>b</strong>");
  });

  it("*em* → <em>", () => {
    const html = renderToHtml("a *e* c");
    expect(html).toContain("<em>e</em>");
  });

  it("~~del~~ → <s>", () => {
    const html = renderToHtml("a ~~d~~ c");
    expect(html).toContain("<s>d</s>");
  });

  it("`code` → <code>", () => {
    const html = renderToHtml("a `c` c");
    expect(html).toContain('<code class="cm-lp-code">c</code>');
  });

  it("[text](url) → <span class=cm-lp-mdlink>", () => {
    const html = renderToHtml("a [t](http://x.com) c");
    expect(html).toContain('class="cm-lp-mdlink"');
    expect(html).toContain(">t</span>");
  });

  it("[[target|label]] → <span class=cm-lp-wikilink>", () => {
    const html = renderToHtml("a [[t|l]] c");
    expect(html).toContain('class="cm-lp-wikilink"');
    expect(html).toContain(">l</span>");
  });

  it("==highlight== → <span class=cm-lp-highlight>", () => {
    const html = renderToHtml("a ==h== c");
    expect(html).toContain('class="cm-lp-highlight"');
    expect(html).toContain(">h</span>");
  });

  it("image ![alt](src) → alt 文本（无样式）", () => {
    const html = renderToHtml("a ![alt](http://x.com/i.png) c");
    expect(html).not.toContain("cm-lp-image");
    expect(html).toContain("alt");
  });

  it("XSS：<script> 原样文本输出", () => {
    const html = renderToHtml("a <script>alert(1)</script> c");
    expect(html).not.toContain("<script>");
    expect(html).toContain("&lt;script&gt;");
  });

  it("链接文本中不解析高亮/粗体（保守处理）→ 纯文本 span", () => {
    const html = renderToHtml("[**b**](http://x.com)");
    // 链接文本 `**b**` 不递归，保持原样
    expect(html).toContain("**b**");
  });

  it("行内码内不解析其他语法", () => {
    const html = renderToHtml("`**not bold**`");
    expect(html).not.toContain("<strong>");
    expect(html).toContain("**not bold**");
  });

  it("未闭合标记原样输出", () => {
    expect(renderToHtml("a **unclosed c")).toContain("**unclosed");
    expect(renderToHtml("a ==unclosed c")).toContain("==unclosed");
  });

  it("链接点击 → window.open 打开", () => {
    const openSpy = vi.spyOn(window, "open").mockImplementation(() => null);
    const frag = renderTableCellInline("[t](http://x.com)", {
      onLink: (url) => window.open(url, "_blank"),
      onWikilink: () => {},
    });
    const host = document.createElement("div");
    host.appendChild(frag);
    const span = host.querySelector(".cm-lp-mdlink") as HTMLElement;
    span.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    expect(openSpy).toHaveBeenCalledWith("http://x.com", "_blank");
    openSpy.mockRestore();
  });

  it("wikilink 点击 → onWikilink 回调", () => {
    const fn = vi.fn();
    const frag = renderTableCellInline("[[target|label]]", {
      onLink: () => {},
      onWikilink: fn,
    });
    const host = document.createElement("div");
    host.appendChild(frag);
    const span = host.querySelector(".cm-lp-wikilink") as HTMLElement;
    span.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    expect(fn).toHaveBeenCalledWith("target");
  });
});

describe("CalloutWidget", () => {
  it("带标题显示图标 + 标题文字", () => {
    const w = new CalloutWidget("note", "My Title");
    const el = w.toDOM() as HTMLElement;
    expect(el.className).toBe("cm-lp-callout-label");
    expect(el.textContent).toContain("📝");
    expect(el.textContent).toContain("My Title");
  });

  it("无标题显示类型显示名（首字母大写）", () => {
    const w = new CalloutWidget("warning", "");
    const el = w.toDOM() as HTMLElement;
    expect(el.textContent).toContain("⚠️");
    expect(el.textContent).toContain("Warning");
  });

  it("未知类型用 📌 图标", () => {
    const w = new CalloutWidget("custom-type", "");
    const el = w.toDOM() as HTMLElement;
    expect(el.textContent).toContain("📌");
    expect(el.textContent).toContain("Custom-type");
  });

  it("eq 比较类型 + 标题", () => {
    expect(new CalloutWidget("note", "t").eq(new CalloutWidget("note", "t"))).toBe(true);
    expect(new CalloutWidget("note", "t").eq(new CalloutWidget("tip", "t"))).toBe(false);
    expect(new CalloutWidget("note", "a").eq(new CalloutWidget("note", "b"))).toBe(false);
  });
});

describe("TableWidget with inline rendering", () => {
  it("单元格 **bold** 渲染为 <strong>（非纯文本）", () => {
    const view = makeView();
    try {
      const w = new TableWidget("| a | b |\n|---|---|\n| **x** | y |");
      const table = w.toDOM(view);
      const td = table.querySelectorAll("tbody td")[0];
      expect(td.querySelector("strong")?.textContent).toBe("x");
    } finally {
      view.destroy();
    }
  });

  it("单元格 ==highlight== 渲染为 highlight span", () => {
    const view = makeView();
    try {
      const w = new TableWidget("| a |\n|---|\n| ==h== |");
      const table = w.toDOM(view);
      const td = table.querySelector("tbody td");
      expect(td?.querySelector(".cm-lp-highlight")?.textContent).toBe("h");
    } finally {
      view.destroy();
    }
  });

  it("单元格 XSS 安全：<img onerror> 原样文本", () => {
    const view = makeView();
    try {
      const w = new TableWidget('| a |\n|---|\n| <img src=x onerror=alert(1)> |');
      const table = w.toDOM(view);
      const td = table.querySelector("tbody td");
      expect(td?.querySelector("img")).toBeNull();
      expect(td?.textContent).toContain("<img");
    } finally {
      view.destroy();
    }
  });

  it("eq 仍只比较 raw", () => {
    const view = makeView();
    try {
      const raw = "| a |\n|---|\n| x |";
      expect(new TableWidget(raw).eq(new TableWidget(raw))).toBe(true);
      expect(new TableWidget(raw).eq(new TableWidget("| b |"))).toBe(false);
    } finally {
      view.destroy();
    }
  });
});

// ── CheckboxWidget eq/点击（jsdom）────────────────────────────────────────────

describe("CheckboxWidget", () => {
  it("eq 不比较位置：checked 相同即相等", () => {
    expect(new CheckboxWidget(true, 0, 3).eq(new CheckboxWidget(true, 10, 13))).toBe(true);
    expect(new CheckboxWidget(false, 0, 3).eq(new CheckboxWidget(false, 0, 3))).toBe(true);
    expect(new CheckboxWidget(true, 0, 3).eq(new CheckboxWidget(false, 0, 3))).toBe(false);
  });

  it("点击翻转走 widgetRangeOf 反查路径", () => {
    const view = makeView();
    try {
      const dispatchSpy = vi.spyOn(view, "dispatch").mockImplementation(() => {});
      const w = new CheckboxWidget(false, 0, 3);
      const el = w.toDOM(view) as HTMLInputElement;
      expect(el.checked).toBe(false);
      el.click();
      // 空文档反查失败 → 回退构造区间 [0,3)，insert [x]
      expect(dispatchSpy).toHaveBeenCalledWith({
        changes: { from: 0, to: 3, insert: "[x]" },
      });
      dispatchSpy.mockRestore();
    } finally {
      view.destroy();
    }
  });
});

describe("widgetRangeOf / revealAtClick", () => {
  it("未挂载 DOM 时回退构造偏移", () => {
    const view = makeView();
    try {
      const dom = document.createElement("span");
      expect(widgetRangeOf(view, dom, 7, 12)).toEqual({ from: 7, to: 12 });
    } finally {
      view.destroy();
    }
  });

  it("已挂载 DOM：反查返回当前位置而非陈旧闭包偏移", () => {
    // 回归：上方编辑后闭包 from 陈旧，点击必须经 DOM 反查映射后位置。
    // 用 EditorView 挂载含 checkbox 的文档，取真实 widget DOM 反查。
    const doc = "- [ ] todo\n";
    const view = new EditorView({
      state: EditorState.create({
        doc,
        extensions: [markdown({ base: markdownLanguage }), livePreview()],
      }),
      parent: document.body,
    });
    try {
      view.dispatch({ selection: { anchor: doc.length } });
      const box = view.dom.querySelector(".cm-lp-checkbox-input") as HTMLElement | null;
      expect(box).not.toBeNull();
      // TaskMarker `[ ]` 在文档 [2,5)：反查返回其当前插入点 2，
      // 而非回退值 999——证明走的是 DOM 反查路径而非闭包/回退
      const at = widgetRangeOf(view, box!, 999, 999);
      expect(at.from).toBe(2);
      expect(at.to).toBe(2);
    } finally {
      view.destroy();
      document.body.innerHTML = "";
    }
  });

  it("revealAtClick 回退时 dispatch 反查/回退位置", () => {
    const view = makeView();
    try {
      const dispatchSpy = vi.spyOn(view, "dispatch").mockImplementation(() => {});
      const focusSpy = vi.spyOn(view, "focus").mockImplementation(() => {});
      const dom = document.createElement("span");
      revealAtClick(view, new MouseEvent("click"), dom, 4, 4);
      expect(dispatchSpy).toHaveBeenCalledWith({ selection: { anchor: 4 } });
      dispatchSpy.mockRestore();
      focusSpy.mockRestore();
    } finally {
      view.destroy();
    }
  });

  it("FrontmatterWidget eq 不比较 from", () => {
    expect(new FrontmatterWidget(2, 0).eq(new FrontmatterWidget(2, 99))).toBe(true);
    expect(new FrontmatterWidget(2, 0).eq(new FrontmatterWidget(3, 0))).toBe(false);
  });

  it("ImageWidget eq 不比较 from（本就如此，锁定）", () => {
    const a = new ImageWidget("alt", "x.png", false, 0, false);
    const b = new ImageWidget("alt", "x.png", false, 50, false);
    expect(a.eq(b)).toBe(true);
  });
});

// ── MathWidget 测试（jsdom）───────────────────────────────────────────────────

describe("MathWidget", () => {
  it("displayMode block → 渲染 .katex 节点 + cm-lp-math-block class", () => {
    const view = makeView();
    try {
      const w = new MathWidget("x^2", true, 0);
      const el = w.toDOM(view) as HTMLElement;
      expect(el.className).toContain("cm-lp-math");
      expect(el.className).toContain("cm-lp-math-block");
      expect(el.querySelector(".katex")).not.toBeNull();
      expect(el.style.cursor).toBe("pointer");
    } finally {
      view.destroy();
    }
  });

  it("inline → .cm-lp-math 不含 block class", () => {
    const view = makeView();
    try {
      const w = new MathWidget("e=mc^2", false, 2);
      const el = w.toDOM(view) as HTMLElement;
      expect(el.className).toBe("cm-lp-math");
      expect(el.querySelector(".katex")).not.toBeNull();
    } finally {
      view.destroy();
    }
  });

  it("点击 → 经 DOM 反查 dispatch（不直接用闭包 from）", () => {
    const view = makeView();
    try {
      const dispatchSpy = vi.spyOn(view, "dispatch").mockImplementation(() => {});
      const w = new MathWidget("x", false, 5);
      const el = w.toDOM(view);
      el.click();
      // 空文档 + 未挂载 DOM：posAtDOM/posAtCoords 反查失败 → 回退构造偏移 5
      // （本用例主要锁定"点击走反查路径"；真实编辑器内反查优先）
      expect(dispatchSpy).toHaveBeenCalledWith({ selection: { anchor: 5 } });
      dispatchSpy.mockRestore();
    } finally {
      view.destroy();
    }
  });

  it("katex 渲染结果缓存：同 tex+displayMode 只调一次 renderToString", () => {
    clearKatexCacheForTest();
    const view = makeView();
    try {
      const a = new MathWidget("x^2+y^2", true, 0);
      const b = new MathWidget("x^2+y^2", true, 10);
      const elA = a.toDOM(view) as HTMLElement;
      const sizeAfterFirst = katexCacheSizeForTest();
      const elB = b.toDOM(view) as HTMLElement;
      expect(katexCacheSizeForTest()).toBe(sizeAfterFirst);
      expect(elA.innerHTML).toBe(elB.innerHTML);
      expect(elA.querySelector(".katex")).not.toBeNull();
    } finally {
      view.destroy();
    }
  });

  it("katex 缓存区分 displayMode", () => {
    clearKatexCacheForTest();
    const view = makeView();
    try {
      const block = new MathWidget("x", true, 0);
      const inline = new MathWidget("x", false, 0);
      const elBlock = block.toDOM(view) as HTMLElement;
      const elInline = inline.toDOM(view) as HTMLElement;
      expect(katexCacheSizeForTest()).toBe(2);
      expect(elBlock.innerHTML).not.toBe(elInline.innerHTML);
    } finally {
      view.destroy();
    }
  });

  it("eq 比较 tex + displayMode", () => {
    const view = makeView();
    try {
      const a = new MathWidget("x", true, 0);
      const b = new MathWidget("x", true, 1); // from 不同，仍相等（eq 不比较 from）
      const c = new MathWidget("x", false, 0);
      const d = new MathWidget("y", true, 0);
      expect(a.eq(b)).toBe(true);
      expect(a.eq(c)).toBe(false);
      expect(a.eq(d)).toBe(false);
    } finally {
      view.destroy();
    }
  });

  it("eq 对非 MathWidget 返回 false", () => {
    const view = makeView();
    try {
      const w = new MathWidget("x", true, 0);
      expect(w.eq(new BulletWidget())).toBe(false);
    } finally {
      view.destroy();
    }
  });
});

// ── BulletWidget 测试（jsdom）────────────────────────────────────────────────

describe("BulletWidget", () => {
  it("toDOM → .cm-lp-bullet + 文本 '•'", () => {
    const el = new BulletWidget().toDOM() as HTMLElement;
    expect(el.className).toBe("cm-lp-bullet");
    expect(el.textContent).toBe("•");
  });

  it("eq 对相同/不同 BulletWidget 结果一致", () => {
    expect(new BulletWidget().eq(new BulletWidget())).toBe(true);
  });

  it("eq 对非 BulletWidget 返回 false", () => {
    const view = makeView();
    try {
      expect(new BulletWidget().eq(new MathWidget("x", false, 0))).toBe(false);
    } finally {
      view.destroy();
    }
  });
});
