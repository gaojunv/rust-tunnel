/**
 * Live Preview 纯函数核心测试（node 环境，无 @codemirror/view）。
 */
import { describe, it, expect } from "vitest";
import { EditorState, EditorSelection } from "@codemirror/state";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { computeLivePreviewSpecs, LP_CLASS, type DecoSpec } from "./spec";

function makeState(doc: string, sel?: { anchor: number; head?: number }[]): EditorState {
  return EditorState.create({
    doc,
    selection:
      sel && sel.length > 0
        ? EditorSelection.create(sel.map((s) => EditorSelection.range(s.anchor, s.head ?? s.anchor)))
        : { anchor: 0 },
    // allowMultipleSelections 否则 EditorState.create 只保留 main range，多选区用例失效
    extensions: [markdown({ base: markdownLanguage }), EditorState.allowMultipleSelections.of(true)],
  });
}

function full(state: EditorState): DecoSpec[] {
  return computeLivePreviewSpecs(state, 0, state.doc.length);
}

function ofKind(specs: DecoSpec[], kind: DecoSpec["kind"]): DecoSpec[] {
  return specs.filter((s) => s.kind === kind);
}

describe("computeLivePreviewSpecs", () => {
  it("空文档返回空", () => {
    const state = makeState("");
    expect(computeLivePreviewSpecs(state, 0, 0)).toEqual([]);
  });

  it("普通段落无装饰", () => {
    const state = makeState("hello world");
    const specs = full(state);
    // 光标在 0：第一行无构造可装饰
    expect(specs).toEqual([]);
  });

  it("ATX 标题：行 class + 隐藏 HeaderMark（含分隔空格）", () => {
    const doc = "# Hello\n";
    // 光标放末尾空行，标题行不被还原
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    expect(specs).toContainEqual({ kind: "line", line: 1, cls: LP_CLASS.h1 });
    expect(specs).toContainEqual({ kind: "hide", from: 0, to: 2 });
  });

  it("ATX 多级别标题", () => {
    const doc = "## H2\n#### H4\n###### H6\n";
    // 光标放文档末尾（空行），三行都不被还原
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    expect(ofKind(specs, "line")).toEqual([
      { kind: "line", line: 1, cls: LP_CLASS.h2 },
      { kind: "line", line: 2, cls: LP_CLASS.h4 },
      { kind: "line", line: 3, cls: LP_CLASS.h6 },
    ]);
    // 隐藏三个 HeaderMark
    expect(ofKind(specs, "hide").length).toBe(3);
  });

  it("ATX 闭合 hash：尾部 HeaderMark 及中间空格一并隐藏", () => {
    const doc = "# Title #\n";
    // 光标放在末尾空行，标题行不被还原
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    expect(specs).toContainEqual({ kind: "line", line: 1, cls: LP_CLASS.h1 });
    // 开标记符 + 分隔空格一起隐藏
    expect(specs).toContainEqual({ kind: "hide", from: 0, to: 2 });
    // " #"：分隔空格 + 闭合 hash 一起隐藏
    expect(specs).toContainEqual({ kind: "hide", from: 7, to: 9 });
  });

  it("ATX 空标题行（仅 #）不产生越界 hide", () => {
    const doc = "#\nbody\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    expect(specs).toContainEqual({ kind: "line", line: 1, cls: LP_CLASS.h1 });
    expect(specs).toContainEqual({ kind: "hide", from: 0, to: 1 });
    // 不吞掉 body 行
    expect(specs.filter((s) => s.kind === "hide").length).toBe(1);
  });

  it("Setext 标题：下划线行隐藏，正文行加 class", () => {
    const doc = "My Title\n========\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    expect(specs).toContainEqual({ kind: "line", line: 1, cls: LP_CLASS.h1 });
    expect(specs).toContainEqual({ kind: "hide", from: 9, to: 17 });
  });

  it("Setext 二级标题", () => {
    const doc = "Sub\n---\n";
    // "---" 此时解析为 SetextHeading2
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = computeLivePreviewSpecs(state, 0, state.doc.length);
    expect(specs).toContainEqual({ kind: "line", line: 1, cls: LP_CLASS.h2 });
  });

  it("粗体/斜体/删除线/行内码：隐藏标记符", () => {
    // 光标放在第二段落，远离所有行内构造（行尾换行符仍在 pad 外扩区内）
    const doc = "**strong** and *em* and ~~del~~ and `code`\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const hides = ofKind(specs, "hide") as { kind: "hide"; from: number; to: number }[];
    const texts = hides.map((h) => doc.slice(h.from, h.to)).sort();
    expect(texts).toEqual(["*", "*", "**", "**", "`", "`", "~~", "~~"]);
  });

  it("行内构造无内容样式 mark（分层：highlight 层负责）", () => {
    const doc = "**strong** and `code`\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "mark")).toEqual([]);
  });

  it("wikilink [[target|label]] 与 [[bare]] 转为 widget spec", () => {
    // 光标放在第二段落，远离 wikilink（行尾换行符仍在 pad=2 外扩区内）
    const doc = "go [[target|label]] and [[bare]]\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    expect(specs).toContainEqual({
      kind: "wikilink",
      from: 3,
      to: 19,
      target: "target",
      label: "label",
    });
    expect(specs).toContainEqual({
      kind: "wikilink",
      from: 24,
      to: 32,
      target: "bare",
      label: "bare",
    });
  });

  it("空 wikilink [[]] 不装饰", () => {
    const doc = "a [[]] b\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "wikilink")).toEqual([]);
  });

  it("任务标记 → checkbox widget（checked/unchecked）", () => {
    const doc = "- [ ] todo\n- [x] done\n- [X] caps\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const boxes = ofKind(specs, "checkbox");
    expect(boxes).toEqual([
      { kind: "checkbox", from: 2, to: 5, checked: false },
      { kind: "checkbox", from: 13, to: 16, checked: true },
      { kind: "checkbox", from: 24, to: 27, checked: true },
    ]);
  });

  it("引用行加 class", () => {
    const doc = "> quote me\n> second\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    expect(ofKind(specs, "line")).toEqual([
      { kind: "line", line: 1, cls: LP_CLASS.quote },
      { kind: "line", line: 2, cls: LP_CLASS.quote },
    ]);
  });

  it("分隔线 → hr widget", () => {
    const doc = "text\n\n---\n";
    const state = makeState(doc, [{ anchor: 1 }]);
    const specs = ofKind(full(state), "hr");
    expect(specs).toEqual([{ kind: "hr", from: 6, to: 9 }]);
  });

  it("有序列表 ListMark 弱化 + 无序列表", () => {
    const doc = "- a\n1. b\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = ofKind(full(state), "mark");
    expect(specs).toEqual([
      { kind: "mark", from: 0, to: 1, cls: LP_CLASS.listMark },
      { kind: "mark", from: 4, to: 6, cls: LP_CLASS.listMark },
    ]);
  });

  it("围栏代码块：每行背景 class；块内 [[...]] 不渲染 wikilink", () => {
    const doc = "```js\n[[not-a-link]]\ncode\n```\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    expect(ofKind(specs, "line").map((s) => (s as { line: number }).line)).toEqual([1, 2, 3, 4]);
    expect(ofKind(specs, "wikilink")).toEqual([]);
  });

  it("行内码内 [[...]] 不渲染 wikilink", () => {
    // 光标放在第二段落，远离 wikilink
    const doc = "a `[[no]]` b [[yes]]\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = ofKind(full(state), "wikilink");
    expect(specs).toHaveLength(1);
    expect(specs[0]).toMatchObject({ target: "yes" });
  });

  it("块级还原：光标在标题行时整行还原（无 line/hide）", () => {
    const doc = "# Hello\nbody\n";
    // 光标在标题行内
    const state = makeState(doc, [{ anchor: 3 }]);
    const specs = full(state);
    expect(specs).toEqual([]);
  });

  it("块级还原：选区跨标题行与正文行都还原", () => {
    const doc = "# Hello\nbody\n";
    const state = makeState(doc, [{ anchor: 2, head: 10 }]);
    expect(full(state)).toEqual([]);
  });

  it("行内还原：光标进入粗体区间即还原标记符", () => {
    const doc = "a **strong** b\n";
    // 光标在 strong 内部
    const state = makeState(doc, [{ anchor: 6 }]);
    expect(ofKind(full(state), "hide")).toEqual([]);
  });

  it("行内边界外扩：光标紧贴标记符外侧不闪烁（仍还原）", () => {
    const doc = "**strong**\n";
    // 光标在开标记符紧邻左侧（位置 0，即 ** 之前）
    const left = makeState(doc, [{ anchor: 0 }]);
    expect(ofKind(full(left), "hide")).toEqual([]);
    // 光标紧贴闭标记符右侧
    const right = makeState(doc, [{ anchor: 10 }]);
    expect(ofKind(full(right), "hide")).toEqual([]);
  });

  it("行内还原：远离时正常隐藏", () => {
    const doc = "**strong**\n\ntail\n";
    // 光标在第三行
    const state = makeState(doc, [{ anchor: doc.length }]);
    const hides = ofKind(full(state), "hide") as { kind: "hide"; from: number; to: number }[];
    expect(hides.map((h) => doc.slice(h.from, h.to))).toEqual(["**", "**"]);
  });

  it("wikilink 还原：光标在 [[ 内时补全不受影响（无 widget）", () => {
    const doc = "see [[tar\n";
    // 光标在 [[ 之后——自研扫描器需要闭合 ]] 才成 widget，未闭合本就无装饰；
    // 此用例覆盖已闭合但光标仍在内部的场景
    const closed = "see [[target]]\n";
    const inside = makeState(closed, [{ anchor: 7 }]);
    expect(ofKind(full(inside), "wikilink")).toEqual([]);
    // 光标在 [[ 正前方（边界外扩 2）：仍还原
    const before = makeState(closed, [{ anchor: 4 }]);
    expect(ofKind(full(before), "wikilink")).toEqual([]);
    // 光标远离：正常渲染（放在第二空行；行尾换行符仍在 pad=2 外扩区内，故不用 closed.length）
    const away = makeState("see [[target]]\n\n\ntail\n", [{ anchor: ("see [[target]]\n\n\ntail\n").length }]);
    expect(ofKind(full(away), "wikilink")).toHaveLength(1);
    void doc;
  });

  it("多选区：任一选区命中即还原", () => {
    const doc = "# One\n# Two\n";
    // 主光标在文档末尾，但第二选区落在第一行标题
    const state = makeState(doc, [{ anchor: doc.length }, { anchor: 2 }]);
    const specs = full(state);
    // 第一行被还原，第二行保留
    expect(specs).not.toContainEqual({ kind: "line", line: 1, cls: LP_CLASS.h1 });
    expect(specs).toContainEqual({ kind: "line", line: 2, cls: LP_CLASS.h1 });
  });

  it("语法树未就绪容错：纯文本扩展下返回空", () => {
    const state = EditorState.create({ doc: "# hi\n**x**\n[[y]]\n" });
    expect(computeLivePreviewSpecs(state, 0, state.doc.length)).toEqual([]);
  });

  it("只计算给定区间 [from, to)", () => {
    const doc = "# A\n# B\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const line2 = state.doc.line(2);
    const specs = computeLivePreviewSpecs(state, line2.from, state.doc.length);
    expect(specs).toContainEqual({ kind: "line", line: 2, cls: LP_CLASS.h1 });
    expect(specs).not.toContainEqual({ kind: "line", line: 1, cls: LP_CLASS.h1 });
  });

  it("Setext 内容行被选中时还原下划线隐藏", () => {
    const doc = "Title\n=====\n";
    const state = makeState(doc, [{ anchor: 2 }]);
    expect(full(state)).toEqual([]);
  });

  it("引用行被选中时该行还原、其余保留", () => {
    const doc = "> a\n> b\n";
    const state = makeState(doc, [{ anchor: 1 }]);
    const specs = full(state);
    expect(specs).not.toContainEqual({ kind: "line", line: 1, cls: LP_CLASS.quote });
    expect(specs).toContainEqual({ kind: "line", line: 2, cls: LP_CLASS.quote });
  });

  it("hr 行被选中时还原为源码", () => {
    const doc = "---\n";
    const state = makeState(doc, [{ anchor: 1 }]);
    expect(ofKind(full(state), "hr")).toEqual([]);
  });

  it("checkbox 行被选中时还原（保留 ListMark 行为一致）", () => {
    const doc = "- [ ] todo\n";
    const state = makeState(doc, [{ anchor: 4 }]);
    const specs = full(state);
    expect(ofKind(specs, "checkbox")).toEqual([]);
    expect(ofKind(specs, "mark")).toEqual([]);
  });
});

// ── 图片渲染测试 ─────────────────────────────────────────────────────────────

describe("image rendering", () => {
  it("block image (standalone paragraph) → image spec with block:true", () => {
    const doc = "![photo](assets/one.jpg)\n\nsome text\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const imgs = ofKind(full(state), "image") as Extract<DecoSpec, { kind: "image" }>[];
    expect(imgs).toHaveLength(1);
    expect(imgs[0]).toMatchObject({
      kind: "image",
      from: 0,
      to: 24,
      alt: "photo",
      src: "assets/one.jpg",
      block: true,
    });
  });

  it("inline image (paragraph with surrounding text) → image spec with block:false", () => {
    const doc = "text ![pic](http://a.com/i.png) more\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const imgs = ofKind(full(state), "image") as Extract<DecoSpec, { kind: "image" }>[];
    expect(imgs).toHaveLength(1);
    expect(imgs[0]).toMatchObject({
      kind: "image",
      from: 5,
      to: 31,
      alt: "pic",
      src: "http://a.com/i.png",
      block: false,
    });
  });

  it("image inside heading → heading handler runs, no image spec emitted", () => {
    const doc = "# T ![a](http://x.com)\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    expect(ofKind(specs, "image")).toEqual([]);
  });

  it("cursor on image line → no image spec (source shown)", () => {
    const doc = "text ![pic](http://a.com/i.png) more\n";
    // cursor inside image range (position 15)
    const state = makeState(doc, [{ anchor: 15 }]);
    expect(ofKind(full(state), "image")).toEqual([]);
  });

  it("angle-bracket URL → src stripped of <>", () => {
    const doc = "![a](<http://x.com/a b.png>)\n\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const imgs = ofKind(full(state), "image") as Extract<DecoSpec, { kind: "image" }>[];
    expect(imgs[0]).toMatchObject({ src: "http://x.com/a b.png" });
  });

  it("image inside code block not rendered as image", () => {
    const doc = "```\n![not-an-image](url)\n```\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "image")).toEqual([]);
  });
});

// ── Markdown 链接渲染测试 ────────────────────────────────────────────────────

describe("markdown link rendering", () => {
  it("inline link → mdlink spec covering the whole node (no hide specs inside)", () => {
    const doc = "A [click here](https://example.com) B\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const links = ofKind(specs, "mdlink") as Extract<DecoSpec, { kind: "mdlink" }>[];
    expect(links).toHaveLength(1);
    expect(links[0]).toMatchObject({
      kind: "mdlink",
      from: 2,
      to: 35,
      text: "click here",
      url: "https://example.com",
    });
    // widget 整体替换 [text](url)，区间内不应再有 hide 装饰（避免 replace 嵌套冲突）
    const hides = ofKind(specs, "hide") as Extract<DecoSpec, { kind: "hide" }>[];
    const insideHides = hides.filter((h) => h.from >= links[0].from && h.to <= links[0].to);
    expect(insideHides).toEqual([]);
  });

  it("cursor on link line → no mdlink spec (source shown)", () => {
    const doc = "A [link](http://x.com) B\n";
    const state = makeState(doc, [{ anchor: 10 }]);
    expect(ofKind(full(state), "mdlink")).toEqual([]);
  });

  it("ref-style link [text][ref] → no mdlink spec", () => {
    const doc = "A [text][ref] B\n\n[ref]: http://x.com\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "mdlink")).toEqual([]);
  });
});

// ── 表格渲染测试 ─────────────────────────────────────────────────────────────

describe("table rendering", () => {
  it("table → table spec with correct raw text", () => {
    const doc = "| a | b |\n|---|---|\n| 1 | 2 |\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const tables = ofKind(full(state), "table") as Extract<DecoSpec, { kind: "table" }>[];
    expect(tables).toHaveLength(1);
    expect(tables[0]).toMatchObject({
      kind: "table",
      from: 0,
      to: 29,
    });
    expect(tables[0].raw).toBe("| a | b |\n|---|---|\n| 1 | 2 |");
  });

  it("cursor inside table → no table spec (source shown)", () => {
    const doc = "| a | b |\n|---|---|\n| 1 | 2 |\n";
    const state = makeState(doc, [{ anchor: 5 }]);
    expect(ofKind(full(state), "table")).toEqual([]);
  });

  it("table with alignment → table spec emitted", () => {
    const doc = "| l | c | r |\n|:---|:---:|---:|\n| x | y | z |\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "table")).toHaveLength(1);
  });

  it("wikilink inside table cell → not rendered as wikilink", () => {
    const doc = "| [[w]] |\n|---|\n| x |\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "wikilink")).toEqual([]);
  });

  it("image inside table cell → not rendered as image", () => {
    const doc = "| ![a](u) |\n|---|\n| x |\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "image")).toEqual([]);
  });
});

// ── 代码块 header/footer 测试 ────────────────────────────────────────────────

describe("code block header/footer", () => {
  it("fenced code with lang → codeheader + codefooter specs", () => {
    const doc = "```js\ncode here\n```\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const headers = ofKind(specs, "codeheader") as Extract<DecoSpec, { kind: "codeheader" }>[];
    expect(headers).toHaveLength(1);
    expect(headers[0]).toMatchObject({ kind: "codeheader", lang: "js" });
    expect(ofKind(specs, "codefooter")).toHaveLength(1);
  });

  it("fenced code without lang → codeheader with empty lang", () => {
    const doc = "```\ncode\n```\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const headers = ofKind(full(state), "codeheader") as Extract<DecoSpec, { kind: "codeheader" }>[];
    expect(headers).toHaveLength(1);
    expect(headers[0]).toMatchObject({ kind: "codeheader", lang: "" });
  });

  it("cursor on closing line → no codefooter (source shown), codeheader still emits", () => {
    const doc = "```js\ncode here\n```\n";
    // cursor on closing ``` line (line 3, position 17)
    const state = makeState(doc, [{ anchor: 17 }]);
    const specs = full(state);
    expect(ofKind(specs, "codeheader")).toHaveLength(1);
    expect(ofKind(specs, "codefooter")).toEqual([]);
  });

  it("cursor on opening line → no codeheader (source shown), codefooter still emits", () => {
    const doc = "```js\ncode here\n```\n";
    const state = makeState(doc, [{ anchor: 2 }]);
    const specs = full(state);
    expect(ofKind(specs, "codeheader")).toEqual([]);
    expect(ofKind(specs, "codefooter")).toHaveLength(1);
  });

  it("cursor inside code body → both header and footer emit (code body not replaced)", () => {
    const doc = "```js\ncode here\n```\n";
    // cursor at position 10 (inside "code here")
    const state = makeState(doc, [{ anchor: 10 }]);
    const specs = full(state);
    expect(ofKind(specs, "codeheader")).toHaveLength(1);
    expect(ofKind(specs, "codefooter")).toHaveLength(1);
    // code block lines still get background class
    expect(ofKind(specs, "line").map((s) => (s as Extract<DecoSpec, { kind: "line" }>).line)).toEqual([1, 2, 3]);
  });

  it("wikilink inside code block → not rendered as wikilink", () => {
    const doc = "```\n[[not-a-link]]\n```\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "wikilink")).toEqual([]);
  });

  it("image syntax inside code block → not rendered as image", () => {
    const doc = "```\n![not-an-image](url)\n```\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "image")).toEqual([]);
  });
});

// ── ![[...]] 嵌入语法测试 ────────────────────────────────────────────────────

describe("embed wikilink ![[...]]", () => {
  it("standalone image embed → image spec with block:true", () => {
    const doc = "![[assets/a.png]]\n\nsome text\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const imgs = ofKind(full(state), "image") as Extract<DecoSpec, { kind: "image" }>[];
    expect(imgs).toHaveLength(1);
    expect(imgs[0]).toMatchObject({
      kind: "image",
      from: 0,
      to: 17,
      alt: "a",
      src: "assets/a.png",
      block: true,
    });
  });

  it("image embed with size suffix → image spec src=assets/b.jpg", () => {
    const doc = "![[b.jpg|300]]\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const imgs = ofKind(full(state), "image") as Extract<DecoSpec, { kind: "image" }>[];
    expect(imgs).toHaveLength(1);
    expect(imgs[0]).toMatchObject({
      kind: "image",
      from: 0,
      to: 14,
      alt: "b",
      src: "assets/b.jpg",
      block: true,
    });
  });

  it("non-image note embed → wikilink spec with from covering !", () => {
    const doc = "![[某笔记]]\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const wikilinks = ofKind(full(state), "wikilink") as Extract<DecoSpec, { kind: "wikilink" }>[];
    expect(wikilinks).toHaveLength(1);
    expect(wikilinks[0]).toMatchObject({
      kind: "wikilink",
      from: 0, // from 覆盖 !（前移 1）
      to: 8,   // ![[某笔记]] 共 8 字符
      target: "某笔记",
      label: "某笔记",
    });
  });

  it("inline image embed → image spec block:false", () => {
    const doc = "文字 ![[x.png]] 文字\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const imgs = ofKind(full(state), "image") as Extract<DecoSpec, { kind: "image" }>[];
    expect(imgs).toHaveLength(1);
    expect(imgs[0]).toMatchObject({
      kind: "image",
      from: 3,   // ! 位置
      to: 13,   // ]] 结束（![[x.png]] 共 10 字符，from=3 → to=13）
      alt: "x",
      src: "assets/x.png",
      block: false,
    });
  });

  it("cursor on embed → no spec (source shown)", () => {
    const doc = "![[a.png]]\n\ntail\n";
    // 光标在 ! 位置
    const state1 = makeState(doc, [{ anchor: 0 }]);
    expect(ofKind(full(state1), "image")).toEqual([]);
    // 光标在 ] 位置（覆盖到 h i 0）
    const state2 = makeState(doc, [{ anchor: 9 }]);
    expect(ofKind(full(state2), "image")).toEqual([]);
  });

  it("image embed inside code block → not rendered", () => {
    const doc = "```\n![[not-an-image.png]]\n```\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "image")).toEqual([]);
  });

  it("regular wikilink inside frontmatter → not rendered as wikilink", () => {
    // frontmatter 内部 [[note]] 不渲染为 widget
    const doc = "---\ntitle: hi\nref: [[note]]\n---\nbody\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "wikilink")).toEqual([]);
  });
});

// ── frontmatter 折叠测试 ──────────────────────────────────────────────────────

describe("frontmatter folding", () => {
  it("valid frontmatter collapsed → single frontmatter spec, no hr/hide inside", () => {
    const doc = "---\ntitle: hi\nauthor: me\n---\nbody\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const fms = ofKind(specs, "frontmatter") as Extract<DecoSpec, { kind: "frontmatter" }>[];
    expect(fms).toHaveLength(1);
    expect(fms[0]).toMatchObject({
      kind: "frontmatter",
      from: 0,
      to: 28,
      propCount: 2,
    });
    // frontmatter 内部不应有 hr（开头 `---` 误判为 HR）
    expect(ofKind(specs, "hr")).toEqual([]);
    // 也不应有 hide（Setext 等其他装饰）
    expect(ofKind(specs, "hide")).toEqual([]);
    // 也不应有 line class
    expect(ofKind(specs, "line")).toEqual([]);
  });

  it("cursor inside frontmatter → no frontmatter spec, no HR false positive", () => {
    const doc = "---\ntitle: hi\n---\nbody\n";
    const state = makeState(doc, [{ anchor: 5 }]); // 光标在 frontmatter 第 2 行
    const specs = full(state);
    expect(ofKind(specs, "frontmatter")).toEqual([]);
    // HR 误判也应被抑制（frontmatter 内部所有装饰都被跳过）
    expect(ofKind(specs, "hr")).toEqual([]);
  });

  it("no closing --- → not detected as frontmatter", () => {
    const doc = "---\ntitle: hi\nbody\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    expect(ofKind(specs, "frontmatter")).toEqual([]);
  });

  it("--- not on line 1 → not detected", () => {
    const doc = "text\n---\ntitle: hi\n---\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    expect(ofKind(specs, "frontmatter")).toEqual([]);
    // 中间 `---` 可能被 lezer 解析为 HR，但 frontmatter 不应干扰它
  });

  it("frontmatter with ... closing → detected", () => {
    const doc = "---\ntitle: hi\n...\nbody\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const fms = ofKind(specs, "frontmatter") as Extract<DecoSpec, { kind: "frontmatter" }>[];
    expect(fms).toHaveLength(1);
    expect(fms[0]).toMatchObject({ propCount: 1 });
  });

  it("single-line frontmatter (--- ... ---) → propCount 0", () => {
    const doc = "---\n---\nbody\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const fms = ofKind(specs, "frontmatter") as Extract<DecoSpec, { kind: "frontmatter" }>[];
    expect(fms).toHaveLength(1);
    expect(fms[0]).toMatchObject({ propCount: 0 });
  });

  it("frontmatter after body text → not detected (--- not on line 1)", () => {
    const doc = "body text\n---\ntitle: hi\n---\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "frontmatter")).toEqual([]);
  });
});

// ── Callout `> [!type]` 渲染测试 ─────────────────────────────────────────────

describe("callout rendering", () => {
  it("basic callout with title → callout spec + hide + line classes", () => {
    const doc = "> [!note] Title\nbody\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const callouts = ofKind(specs, "callout") as Extract<DecoSpec, { kind: "callout" }>[];
    expect(callouts).toHaveLength(1);
    expect(callouts[0]).toMatchObject({
      kind: "callout",
      calloutType: "note",
      title: "Title",
    });
    // hide 覆盖 `[!note]` 标记区间（不含标题文字）
    const hides = ofKind(specs, "hide") as Extract<DecoSpec, { kind: "hide" }>[];
    const markerHides = hides.filter((h) => doc.slice(h.from, h.to) === "[!note]");
    expect(markerHides).toHaveLength(1);
    // 行 class 含 callout
    const lineSpecs = ofKind(specs, "line") as Extract<DecoSpec, { kind: "line" }>[];
    expect(lineSpecs.some((l) => l.cls.includes("cm-lp-callout"))).toBe(true);
    expect(lineSpecs.some((l) => l.cls.includes("cm-lp-callout-note"))).toBe(true);
  });

  it("callout without title → callout spec with empty title", () => {
    const doc = "> [!warning]\n> content\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const callouts = ofKind(specs, "callout") as Extract<DecoSpec, { kind: "callout" }>[];
    expect(callouts).toHaveLength(1);
    expect(callouts[0]).toMatchObject({ calloutType: "warning", title: "" });
    // 第二行无 callout class
    const lineSpecs = ofKind(specs, "line") as Extract<DecoSpec, { kind: "line" }>[];
    expect(lineSpecs.some((l) => l.line === 2 && l.cls.includes("cm-lp-callout"))).toBe(true);
  });

  it("multiline callout → all lines get callout class", () => {
    const doc = "> [!note]\n> line1\n> line2\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const lineSpecs = ofKind(specs, "line") as Extract<DecoSpec, { kind: "line" }>[];
    const calloutLines = lineSpecs.filter((l) => l.cls.includes("cm-lp-callout-note"));
    expect(calloutLines.map((l) => l.line)).toEqual([1, 2, 3]);
  });

  it("cursor on callout first line → no callout spec (restored)", () => {
    const doc = "> [!note] Title\n> content\n";
    const state = makeState(doc, [{ anchor: 3 }]); // 光标在 callout 首行
    const specs = full(state);
    expect(ofKind(specs, "callout")).toEqual([]);
    // 首行连 quote class 也不应有（整块还原）
    const lineSpecs = ofKind(specs, "line") as Extract<DecoSpec, { kind: "line" }>[];
    expect(lineSpecs.some((l) => l.line === 1)).toBe(false);
  });

  it("non-first-line [!x] not recognized as callout", () => {
    const doc = "> line1\n> [!note] here\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    // 第二行不是 callout 首行，不识别
    expect(ofKind(specs, "callout")).toEqual([]);
    // 但第二行仍有 quote class
    const lineSpecs = ofKind(specs, "line") as Extract<DecoSpec, { kind: "line" }>[];
    expect(lineSpecs).toContainEqual({ kind: "line", line: 2, cls: LP_CLASS.quote });
  });

  it("callout with expand marker + is sanitized type", () => {
    const doc = "> [!tip]+\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const callouts = ofKind(specs, "callout") as Extract<DecoSpec, { kind: "callout" }>[];
    expect(callouts[0]).toMatchObject({ calloutType: "tip", title: "" });
  });

  it("callout type sanitized: non-alnum chars stripped", () => {
    const doc = "> [!My-Note!]\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const callouts = ofKind(specs, "callout") as Extract<DecoSpec, { kind: "callout" }>[];
    expect(callouts[0]).toMatchObject({ calloutType: "my-note" });
  });

  it("plain quote (no callout marker) → no callout spec", () => {
    const doc = "> just a quote\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    expect(ofKind(specs, "callout")).toEqual([]);
    expect(ofKind(specs, "line")).toEqual([{ kind: "line", line: 1, cls: LP_CLASS.quote }]);
  });
});

// ── ==highlight== 渲染测试 ───────────────────────────────────────────────────

describe("==highlight== rendering", () => {
  it("basic highlight pair → hide + highlight mark", () => {
    const doc = "==hl==\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const highlights = ofKind(specs, "highlight") as Extract<DecoSpec, { kind: "highlight" }>[];
    expect(highlights).toHaveLength(1);
    expect(highlights[0]).toMatchObject({
      from: 2,
      to: 4,
      markFrom: 0,
      markTo: 4,
    });
    // 两个 `==` 都有 hide（positions 0-2, 4-6）
    const hides = ofKind(specs, "hide") as Extract<DecoSpec, { kind: "hide" }>[];
    const eqHides = hides.filter((h) => doc.slice(h.from, h.to) === "==");
    expect(eqHides).toHaveLength(2);
  });

  it("highlight inside inline code → not rendered", () => {
    const doc = "`==not hl==`\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "highlight")).toEqual([]);
  });

  it("unclosed == → not rendered", () => {
    const doc = "==unclosed\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "highlight")).toEqual([]);
  });

  it("cursor touches highlight content (pad=2) → restored", () => {
    const doc = "a ==hl== b\n\ntail\n";
    // 光标在 `hl` 内部（位置 4）
    const state = makeState(doc, [{ anchor: 4 }]);
    expect(ofKind(full(state), "highlight")).toEqual([]);
  });

  it("cursor touches highlight mark boundary → restored (pad=2)", () => {
    const doc = "a ==hl== b\n\ntail\n";
    // 光标紧贴 `==` 开标记右侧（位置 3）
    const state = makeState(doc, [{ anchor: 3 }]);
    expect(ofKind(full(state), "highlight")).toEqual([]);
  });

  it("cursor away → highlight rendered", () => {
    const doc = "==hl==\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const highlights = ofKind(full(state), "highlight") as Extract<DecoSpec, { kind: "highlight" }>[];
    expect(highlights).toHaveLength(1);
  });

  it("multiple highlights on same line", () => {
    const doc = "==a== and ==b==\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const highlights = ofKind(full(state), "highlight") as Extract<DecoSpec, { kind: "highlight" }>[];
    expect(highlights).toHaveLength(2);
  });

  it("empty highlight == == → not rendered", () => {
    const doc = "====\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "highlight")).toEqual([]);
  });

  it("=== not treated as highlight (triple =)", () => {
    const doc = "===not hl===\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "highlight")).toEqual([]);
  });

  it("highlight inside table cell → not rendered (table widget handles)", () => {
    const doc = "| ==hl== |\n|---|\n| x |\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "highlight")).toEqual([]);
  });

  it("highlight in frontmatter → not rendered", () => {
    const doc = "---\ntitle: ==no==\n---\nbody\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    expect(ofKind(full(state), "highlight")).toEqual([]);
  });
});

// ── 标题内行内构造测试 ───────────────────────────────────────────────────────

describe("inline constructs inside headings", () => {
  it("heading with bold + link → header mark hide + bold hide + mdlink spec", () => {
    const doc = "# Title **bold** [link](http://x.com)\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    // 标题行 class
    expect(specs).toContainEqual({ kind: "line", line: 1, cls: LP_CLASS.h1 });
    // HeaderMark hide（`# `）
    expect(specs).toContainEqual({ kind: "hide", from: 0, to: 2 });
    // 粗体标记 hide（两个 `**`）
    const hides = ofKind(specs, "hide") as Extract<DecoSpec, { kind: "hide" }>[];
    const boldHides = hides.filter((h) => doc.slice(h.from, h.to) === "**");
    expect(boldHides).toHaveLength(2);
    // mdlink spec
    const links = ofKind(specs, "mdlink") as Extract<DecoSpec, { kind: "mdlink" }>[];
    expect(links).toHaveLength(1);
    expect(links[0]).toMatchObject({ text: "link", url: "http://x.com" });
  });

  it("cursor on heading line → all restored (no inline specs)", () => {
    const doc = "# Title **bold**\n";
    const state = makeState(doc, [{ anchor: 3 }]); // 光标在标题行
    const specs = full(state);
    expect(specs).toEqual([]);
  });

  it("setext heading with bold → bold marks hidden when cursor away", () => {
    const doc = "Title **bold**\n===\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    // 标题行 class
    expect(specs).toContainEqual({ kind: "line", line: 1, cls: LP_CLASS.h1 });
    // 粗体标记 hide
    const hides = ofKind(specs, "hide") as Extract<DecoSpec, { kind: "hide" }>[];
    const boldHides = hides.filter((h) => doc.slice(h.from, h.to) === "**");
    expect(boldHides).toHaveLength(2);
  });

  it("heading with inline code → code ranges tracked, no hide from highlight scan", () => {
    const doc = "# Title `code`\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    // 标题行 class
    expect(specs).toContainEqual({ kind: "line", line: 1, cls: LP_CLASS.h1 });
    // 行内码标记 hide（` 反引号）
    const hides = ofKind(specs, "hide") as Extract<DecoSpec, { kind: "hide" }>[];
    const codeHides = hides.filter((h) => doc.slice(h.from, h.to) === "`");
    expect(codeHides).toHaveLength(2);
  });

  it("heading with wikilink → wikilink spec emitted", () => {
    const doc = "# [[target|label]]\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const wikilinks = ofKind(specs, "wikilink") as Extract<DecoSpec, { kind: "wikilink" }>[];
    expect(wikilinks).toHaveLength(1);
    expect(wikilinks[0]).toMatchObject({ target: "target", label: "label" });
  });

  it("heading with highlight → highlight rendered inside heading", () => {
    const doc = "# Title ==hl==\n\ntail\n";
    const state = makeState(doc, [{ anchor: doc.length }]);
    const specs = full(state);
    const highlights = ofKind(specs, "highlight") as Extract<DecoSpec, { kind: "highlight" }>[];
    expect(highlights).toHaveLength(1);
  });
});
