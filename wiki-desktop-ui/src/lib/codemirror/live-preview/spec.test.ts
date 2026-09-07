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
