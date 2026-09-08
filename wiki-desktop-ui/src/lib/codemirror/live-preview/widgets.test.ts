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
import {
  CalloutWidget,
  renderTableCellInline,
  TableWidget,
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
      const w = new TableWidget("| a | b |\n|---|---|\n| **x** | y |", view);
      const table = w.toDOM();
      const td = table.querySelectorAll("tbody td")[0];
      expect(td.querySelector("strong")?.textContent).toBe("x");
    } finally {
      view.destroy();
    }
  });

  it("单元格 ==highlight== 渲染为 highlight span", () => {
    const view = makeView();
    try {
      const w = new TableWidget("| a |\n|---|\n| ==h== |", view);
      const table = w.toDOM();
      const td = table.querySelector("tbody td");
      expect(td?.querySelector(".cm-lp-highlight")?.textContent).toBe("h");
    } finally {
      view.destroy();
    }
  });

  it("单元格 XSS 安全：<img onerror> 原样文本", () => {
    const view = makeView();
    try {
      const w = new TableWidget('| a |\n|---|\n| <img src=x onerror=alert(1)> |', view);
      const table = w.toDOM();
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
      expect(new TableWidget(raw, view).eq(new TableWidget(raw, view))).toBe(true);
      expect(new TableWidget(raw, view).eq(new TableWidget("| b |", view))).toBe(false);
    } finally {
      view.destroy();
    }
  });
});
