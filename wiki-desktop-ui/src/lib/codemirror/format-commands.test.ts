import { describe, it, expect } from "vitest";
import { EditorState } from "@codemirror/state";
import {
  insertImageMarkdown,
  insertLinkPure,
  insertTablePure,
  setHeadingOnLines,
  setListOnLines,
  toggleInlineMarker,
} from "./format-commands";

function stateWithSelection(doc: string, anchor: number, head?: number): EditorState {
  return EditorState.create({
    doc,
    selection: head === undefined ? { anchor } : { anchor, head },
  });
}

function apply(
  state: EditorState,
  result: NonNullable<ReturnType<typeof toggleInlineMarker>>,
): { doc: string; anchor: number; head: number } {
  const tr = state.update({ changes: result.changes, selection: result.selection });
  return {
    doc: tr.state.doc.toString(),
    anchor: tr.state.selection.main.anchor,
    head: tr.state.selection.main.head,
  };
}

// helpers for heading/list: setHeadingOnLines and setListOnLines return PureResult
function applyPure(
  state: EditorState,
  result: NonNullable<ReturnType<typeof setHeadingOnLines>>,
) {
  return apply(state, result);
}

describe("toggleInlineMarker / bold italic", () => {
  it("wrap non-empty selection with **", () => {
    const s = stateWithSelection("hello world", 6, 11); // "world"
    const r = toggleInlineMarker(s, "**")!;
    expect(r).not.toBeNull();
    const out = apply(s, r);
    expect(out.doc).toBe("hello **world**");
    expect(out.doc.slice(out.anchor, out.head)).toBe("world");
  });

  it("unwrap selection that already wrapped (case A)", () => {
    const s = stateWithSelection("a **world** b", 2, 11); // "**world**"
    const r = toggleInlineMarker(s, "**")!;
    const out = apply(s, r);
    expect(out.doc).toBe("a world b");
    expect(out.doc.slice(out.anchor, out.head)).toBe("world");
  });

  it("unwrap markers outside selection (case B)", () => {
    const doc = "a **world** b";
    // selection is "world" at 4..9 (inside markers)
    const worldStart = doc.indexOf("world");
    const worldEnd = worldStart + 5;
    const s = stateWithSelection(doc, worldStart, worldEnd);
    const r = toggleInlineMarker(s, "**")!;
    const out = apply(s, r);
    expect(out.doc).toBe("a world b");
  });

  it("empty selection inserts placeholder and selects it (bold)", () => {
    const s = stateWithSelection("hello ", 6);
    const r = toggleInlineMarker(s, "**")!;
    const out = apply(s, r);
    expect(out.doc).toBe("hello **粗体**");
    expect(out.doc.slice(out.anchor, out.head)).toBe("粗体");
  });

  it("empty selection placeholder for italic", () => {
    const s = stateWithSelection("", 0);
    const r = toggleInlineMarker(s, "*")!;
    const out = apply(s, r);
    expect(out.doc).toBe("*斜体*");
    expect(out.doc.slice(out.anchor, out.head)).toBe("斜体");
  });

  it("empty selection placeholder for inline code", () => {
    const s = stateWithSelection("x", 1);
    const r = toggleInlineMarker(s, "`")!;
    const out = apply(s, r);
    expect(out.doc).toBe("x`代码`");
  });

  it("cursor inside **bold** unwraps", () => {
    const doc = "a **hello** b";
    const pos = doc.indexOf("hello") + 2; // inside
    const s = stateWithSelection(doc, pos);
    const r = toggleInlineMarker(s, "**")!;
    const out = apply(s, r);
    expect(out.doc).toBe("a hello b");
  });

  it("cursor inside *italic* unwraps", () => {
    const doc = "a *hello* b";
    const pos = doc.indexOf("hello") + 1;
    const s = stateWithSelection(doc, pos);
    const r = toggleInlineMarker(s, "*")!;
    const out = apply(s, r);
    expect(out.doc).toBe("a hello b");
  });

  it("italic does not interfere with bold", () => {
    const doc = "a **hello** b";
    const pos = doc.indexOf("hello") + 1; // inside bold, not italic
    const s = stateWithSelection(doc, pos);
    const r = toggleInlineMarker(s, "*")!;
    // should insert *斜体* at cursor rather than unwrap bold
    const out = apply(s, r);
    expect(out.doc).toBe("a **h*斜体*ello** b");
    expect(out.doc).toContain("h");
    expect(out.doc).toContain("ello");
  });

  it("strikethrough wrap placeholder", () => {
    const s = stateWithSelection("", 0);
    const r = toggleInlineMarker(s, "~~")!;
    const out = apply(s, r);
    expect(out.doc).toBe("~~删除线~~");
  });

  it("multi-line selection wraps whole range", () => {
    const doc = "line1\nline2";
    const s = stateWithSelection(doc, 0, doc.length);
    const r = toggleInlineMarker(s, "**")!;
    const out = apply(s, r);
    expect(out.doc).toBe("**line1\nline2**");
  });
});

describe("setHeadingOnLines", () => {
  it("adds heading prefix to single line", () => {
    const s = stateWithSelection("hello", 0);
    const r = setHeadingOnLines(s, 1)!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("# hello");
  });

  it("strips different level and replaces", () => {
    const s = stateWithSelection("## hello", 0);
    const r = setHeadingOnLines(s, 1)!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("# hello");
  });

  it("toggle off when already same level", () => {
    const s = stateWithSelection("# hello", 0);
    const r = setHeadingOnLines(s, 1)!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("hello");
  });

  it("multi-line heading apply and toggle off", () => {
    const doc = "a\nb\nc";
    const s = stateWithSelection(doc, 0, doc.length);
    const r = setHeadingOnLines(s, 2)!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("## a\n## b\n## c");
    // toggle off: apply again from resulting state
    const s2 = EditorState.create({ doc: out.doc, selection: { anchor: 0, head: out.doc.length } });
    const r2 = setHeadingOnLines(s2, 2)!;
    const out2 = applyPure(s2, r2);
    expect(out2.doc).toBe("a\nb\nc");
  });

  it("only affects selected lines", () => {
    const doc = "line1\nline2\nline3";
    // select only line2
    const line2Start = doc.indexOf("line2");
    const s = stateWithSelection(doc, line2Start, line2Start + 5);
    const r = setHeadingOnLines(s, 3)!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("line1\n### line2\nline3");
  });

  it("does not add extra prefix on empty line (still toggles)", () => {
    const s = stateWithSelection("", 0);
    const r = setHeadingOnLines(s, 1)!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("# ");
  });
});

describe("setListOnLines", () => {
  it("bullet: adds - to plain lines", () => {
    const doc = "a\nb";
    const s = stateWithSelection(doc, 0, doc.length);
    const r = setListOnLines(s, "bullet")!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("- a\n- b");
  });

  it("bullet: toggle off removes -", () => {
    const doc = "- a\n- b";
    const s = stateWithSelection(doc, 0, doc.length);
    const r = setListOnLines(s, "bullet")!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("a\nb");
  });

  it("bullet: preserves leading whitespace", () => {
    const doc = "  hello";
    const s = stateWithSelection(doc, 0, doc.length);
    const r = setListOnLines(s, "bullet")!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("  - hello");
  });

  it("ordered: renumbered sequentially", () => {
    const doc = "a\nb\nc";
    const s = stateWithSelection(doc, 0, doc.length);
    const r = setListOnLines(s, "ordered")!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("1. a\n2. b\n3. c");
  });

  it("ordered: toggle off strips numbers", () => {
    const doc = "1. a\n2. b\n7. c";
    const s = stateWithSelection(doc, 0, doc.length);
    const r = setListOnLines(s, "ordered")!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("a\nb\nc");
  });

  it("ordered: mixed lines get renumbered", () => {
    const doc = "1. already\nplain";
    const s = stateWithSelection(doc, 0, doc.length);
    const r = setListOnLines(s, "ordered")!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("1. already\n2. plain");
  });

  it("task: adds - [ ] prefix", () => {
    const doc = "todo";
    const s = stateWithSelection(doc, 0, doc.length);
    const r = setListOnLines(s, "task")!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("- [ ] todo");
  });

  it("task: toggle off removes task marker", () => {
    const doc = "- [ ] todo\n- [x] done";
    const s = stateWithSelection(doc, 0, doc.length);
    const r = setListOnLines(s, "task")!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("todo\ndone");
  });

  it("task: preserves indentation", () => {
    const doc = "  item";
    const s = stateWithSelection(doc, 0, doc.length);
    const r = setListOnLines(s, "task")!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("  - [ ] item");
  });
});

describe("insertTablePure", () => {
  it("inserts table at start with cursor in first header cell", () => {
    const s = stateWithSelection("", 0);
    const r = insertTablePure(s)!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("| Header 1 | Header 2 | Header 3 |\n| --- | --- | --- |\n|  |  |  |\n|  |  |  |");
    expect(out.doc.slice(out.anchor, out.head)).toBe("Header 1");
  });

  it("ensures blank line before when not at line start", () => {
    const doc = "hello";
    const s = stateWithSelection(doc, doc.length);
    const r = insertTablePure(s)!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("hello\n\n| Header 1 | Header 2 | Header 3 |\n| --- | --- | --- |\n|  |  |  |\n|  |  |  |");
    // cursor still selects Header 1
    expect(out.doc.slice(out.anchor, out.head)).toBe("Header 1");
  });

  it("does not add blank line when at line start (empty line)", () => {
    const doc = "line1\n";
    const s = stateWithSelection(doc, doc.length);
    const r = insertTablePure(s)!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("line1\n| Header 1 | Header 2 | Header 3 |\n| --- | --- | --- |\n|  |  |  |\n|  |  |  |");
  });
});

describe("insertLinkPure", () => {
  it("wrap non-empty selection as [sel](url) and select url", () => {
    const doc = "hello world";
    const s = stateWithSelection(doc, 6, 11); // world
    const r = insertLinkPure(s)!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("hello [world](url)");
    expect(out.doc.slice(out.anchor, out.head)).toBe("url");
  });

  it("empty selection inserts [标题](url) template and selects 标题", () => {
    const s = stateWithSelection("a", 1);
    const r = insertLinkPure(s)!;
    const out = applyPure(s, r);
    expect(out.doc).toBe("a[标题](url)");
    expect(out.doc.slice(out.anchor, out.head)).toBe("标题");
  });
});

describe("insertImageMarkdown", () => {
  it("builds markdown image string", () => {
    expect(insertImageMarkdown("path/to/img.png")).toBe("![image](path/to/img.png)");
    expect(insertImageMarkdown("a/b.jpg", "alt text")).toBe("![alt text](a/b.jpg)");
    expect(insertImageMarkdown("", "x")).toBe("![x]()");
  });
});
