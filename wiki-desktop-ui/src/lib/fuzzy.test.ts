import { describe, it, expect } from "vitest";
import { fuzzyScore, fuzzyScorePrepared, matchIndices, prepareHaystack } from "./fuzzy";

describe("fuzzyScorePrepared 与 fuzzyScore 一致性", () => {
  const cases: Array<[string, string]> = [
    ["Hello World", "hw"],
    ["Hello World", "hello"],
    ["foo/bar-baz_qux.md", "fbq"],
    ["笔记/子目录/测试", "测试"],
    ["a/b/c", "abc"],
    ["abc", "xyz"],
    ["abc", ""],
    ["", "a"],
    ["Wiki Desktop", "wd"],
  ];
  for (const [candidate, query] of cases) {
    it(`"${candidate}" × "${query}"`, () => {
      const expected = fuzzyScore(candidate, query);
      const hay = prepareHaystack(candidate);
      expect(fuzzyScorePrepared(hay, query.toLowerCase())).toBe(expected);
    });
  }

  it("大小写不敏感：query 大小写混合与小写结果一致", () => {
    const hay = prepareHaystack("Hello World");
    expect(fuzzyScorePrepared(hay, "HW".toLowerCase())).toBe(fuzzyScore("Hello World", "HW"));
  });

  it("空 query 返回 0（与 fuzzyScore 一致）", () => {
    expect(fuzzyScorePrepared(prepareHaystack("abc"), "")).toBe(0);
  });

  it("连续/边界加分一致：word boundary 命中高于普通子序列", () => {
    const a = fuzzyScorePrepared(prepareHaystack("foo bar"), "fb");
    const b = fuzzyScorePrepared(prepareHaystack("fxoxbxaxr"), "fb");
    expect(a).not.toBeNull();
    expect(b).not.toBeNull();
    expect(a!).toBeGreaterThan(b!);
  });
});

describe("matchIndices", () => {
  it("空 query 返回空数组", () => {
    expect(matchIndices("abc", "")).toEqual([]);
  });

  it("子序列按顺序匹配", () => {
    expect(matchIndices("abc", "ac")).toEqual([0, 2]);
  });

  it("失配返回空数组", () => {
    expect(matchIndices("abc", "z")).toEqual([]);
  });
});
