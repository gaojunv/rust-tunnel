// 文件夹树纯逻辑单测
import { describe, it, expect } from "vitest";
import { buildTree, folderPathsOf } from "./folder-tree";
import type { NoteSummary } from "@/api/types";

function note(key: string, overrides?: Partial<NoteSummary>): NoteSummary {
  return {
    key,
    title: overrides?.title ?? key,
    tags: overrides?.tags ?? [],
    modified: overrides?.modified ?? 1000,
  };
}

describe("folderPathsOf", () => {
  it("单段 key 无祖先", () => {
    expect(folderPathsOf("note")).toEqual([]);
  });
  it("两段", () => {
    expect(folderPathsOf("a/b")).toEqual(["a"]);
  });
  it("三段", () => {
    expect(folderPathsOf("a/b/c")).toEqual(["a", "a/b"]);
  });
  it("多段", () => {
    expect(folderPathsOf("a/b/c/d")).toEqual(["a", "a/b", "a/b/c"]);
  });
  it("空串", () => {
    expect(folderPathsOf("")).toEqual([]);
  });
});

describe("buildTree", () => {
  it("空列表返回空", () => {
    expect(buildTree([])).toEqual([]);
  });

  it("无文件夹时退化为平铺笔记列表并按 name 排序", () => {
    const nodes = buildTree([note("b"), note("a"), note("c")]);
    expect(nodes.map((n) => (n.kind === "note" ? n.name : n.kind))).toEqual(["a", "b", "c"]);
    expect(nodes.every((n) => n.kind === "note")).toBe(true);
  });

  it("单层文件夹嵌套", () => {
    const nodes = buildTree([note("a/x"), note("a/y"), note("b/z")]);
    // 顶层应为两个文件夹 a、b
    expect(nodes.length).toBe(2);
    expect(nodes[0].kind).toBe("folder");
    expect(nodes[1].kind).toBe("folder");
    if (nodes[0].kind === "folder") {
      expect(nodes[0].name).toBe("a");
      expect(nodes[0].path).toBe("a");
      expect(nodes[0].noteCount).toBe(2);
      expect(nodes[0].children.length).toBe(2);
    }
    if (nodes[1].kind === "folder") {
      expect(nodes[1].name).toBe("b");
      expect(nodes[1].noteCount).toBe(1);
    }
  });

  it("多层嵌套且递归计数", () => {
    const nodes = buildTree([note("a/b/c"), note("a/b/d"), note("a/e")]);
    // 顶层只有 a
    expect(nodes.length).toBe(1);
    const a = nodes[0];
    expect(a.kind).toBe("folder");
    if (a.kind === "folder") {
      expect(a.noteCount).toBe(3);
      // a 的子节点：文件夹 b 在前，笔记 e 在后
      expect(a.children.length).toBe(2);
      expect(a.children[0].kind).toBe("folder");
      expect(a.children[1].kind).toBe("note");
      const b = a.children[0];
      if (b.kind === "folder") {
        expect(b.name).toBe("b");
        expect(b.path).toBe("a/b");
        expect(b.noteCount).toBe(2);
      }
    }
  });

  it("文件夹在前、各自按 name.localeCompare 排序（英文）", () => {
    const nodes = buildTree([note("z"), note("a/note"), note("b/note"), note("a")]);
    // 顶层：文件夹 a、b 在前，笔记 a、z 在后（按 name 排序）
    // 注意：顶层文件夹按 name 排序，笔记按 name 排序，文件夹整体在前
    const kinds = nodes.map((n) => n.kind);
    expect(kinds).toEqual(["folder", "folder", "note", "note"]);
    const names = nodes.map((n) => n.name);
    expect(names).toEqual(["a", "b", "a", "z"]);
  });

  it("中英文混排按 zh locale 排序", () => {
    const nodes = buildTree([note("文件夹/笔记"), note("a/note"), note("文件夹/a"), note("b/note")]);
    // 顶层文件夹为 a、b、文件夹（按 zh locale，字母应在中文前）
    const folderNames = nodes.filter((n) => n.kind === "folder").map((n) => n.name);
    // 期望字母文件夹排在中文文件夹前
    const expectedSorted = [...folderNames].sort((x, y) => x.localeCompare(y, "zh"));
    expect(folderNames).toEqual(expectedSorted);
    expect(folderNames.slice(0, -1)).toEqual([...folderNames.slice(0, -1)].sort((x, y) => x.localeCompare(y, "zh")));
  });

  it("同文件夹下笔记按名称排序", () => {
    const nodes = buildTree([note("a/zebra"), note("a/apple"), note("a/banana")]);
    const a = nodes[0];
    expect(a.kind).toBe("folder");
    if (a.kind === "folder") {
      const noteNames = a.children.filter((c) => c.kind === "note").map((c) => c.name);
      const sorted = [...noteNames].sort((x, y) => x.localeCompare(y, "zh"));
      expect(noteNames).toEqual(sorted);
    }
  });

  it("同级文件夹与笔记混排：文件夹在前", () => {
    const nodes = buildTree([note("a/note"), note("root-note")]);
    expect(nodes[0].kind).toBe("folder");
    expect(nodes[1].kind).toBe("note");
  });

  it("深层同名校验：noteCount 递归准确", () => {
    const nodes = buildTree([note("a/b/c"), note("a/b/d"), note("a/b/e/f")]);
    const a = nodes[0];
    if (a.kind !== "folder") throw new Error("expected folder");
    expect(a.noteCount).toBe(3);
    const b = a.children.find((c) => c.kind === "folder" && c.name === "b");
    if (!b || b.kind !== "folder") throw new Error("expected b");
    expect(b.noteCount).toBe(3);
  });
});
