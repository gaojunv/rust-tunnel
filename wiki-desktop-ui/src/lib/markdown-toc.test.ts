import { describe, it, expect } from "vitest";
import { extractToc, resolveActiveTocIndex } from "./markdown-toc";

describe("extractToc", () => {
  it("多级标题", () => {
    const body = "# 一级\n## 二级\n### 三级\n#### 四级";
    const toc = extractToc(body);
    expect(toc).toEqual([
      { level: 1, text: "一级", line: 0 },
      { level: 2, text: "二级", line: 1 },
      { level: 3, text: "三级", line: 2 },
      { level: 4, text: "四级", line: 3 },
    ]);
  });

  it("代码块内 # 不算", () => {
    const body = "# 真标题\n```\n# 假标题\n## 也假\n```\n## 真二级";
    const toc = extractToc(body);
    expect(toc).toEqual([
      { level: 1, text: "真标题", line: 0 },
      { level: 2, text: "真二级", line: 5 },
    ]);
  });

  it("尾部 # 闭合", () => {
    const body = "## 标题 ##\n# 另一个 #";
    const toc = extractToc(body);
    expect(toc).toEqual([
      { level: 2, text: "标题", line: 0 },
      { level: 1, text: "另一个", line: 1 },
    ]);
  });

  it("无标题空数组", () => {
    expect(extractToc("正文无标题\n只有段落")).toEqual([]);
    expect(extractToc("")).toEqual([]);
  });

  it("~~~ 围栏也跳过", () => {
    const body = "# a\n~~~\n# 假\n~~~\n## b";
    const toc = extractToc(body);
    expect(toc).toEqual([
      { level: 1, text: "a", line: 0 },
      { level: 2, text: "b", line: 4 },
    ]);
  });

  it("# 后无空格不算标题", () => {
    const body = "#标题\n# 真标题";
    const toc = extractToc(body);
    expect(toc).toEqual([{ level: 1, text: "真标题", line: 1 }]);
  });
});

describe("resolveActiveTocIndex", () => {
  it("光标在首个标题前返回 -1", () => {
    const toc = extractToc("前言\n# A\n正文\n## B");
    expect(resolveActiveTocIndex(toc, 0)).toBe(-1);
  });

  it("命中最后一个 line <= 光标行的标题", () => {
    const toc = extractToc("# A\n正文\n正文\n## B\n正文");
    expect(resolveActiveTocIndex(toc, 0)).toBe(0);
    expect(resolveActiveTocIndex(toc, 2)).toBe(0);
    expect(resolveActiveTocIndex(toc, 3)).toBe(1);
    expect(resolveActiveTocIndex(toc, 99)).toBe(1);
  });

  it("cursorLine 为 null 或空大纲返回 -1", () => {
    const toc = extractToc("# A");
    expect(resolveActiveTocIndex(toc, null)).toBe(-1);
    expect(resolveActiveTocIndex([], 5)).toBe(-1);
  });

  it("光标恰好落在标题行时命中该标题", () => {
    const toc = extractToc("# A\n## B\n### C");
    expect(resolveActiveTocIndex(toc, 1)).toBe(1);
    expect(resolveActiveTocIndex(toc, 2)).toBe(2);
  });
});
