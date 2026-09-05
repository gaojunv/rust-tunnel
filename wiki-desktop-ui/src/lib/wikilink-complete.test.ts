import { describe, it, expect } from "vitest";
import { buildInsertion, findLinkQuery } from "./wikilink-complete";

describe("findLinkQuery", () => {
  it("正常：提取 query", () => {
    const text = "hello [[foo";
    expect(findLinkQuery(text, text.length)).toEqual({ start: 6, query: "foo" });
  });

  it("空 query", () => {
    const text = "hi [[";
    expect(findLinkQuery(text, text.length)).toEqual({ start: 3, query: "" });
  });

  it("无 [[ 返回 null", () => {
    expect(findLinkQuery("hello world", 5)).toBeNull();
  });

  it("已闭合返回 null", () => {
    const text = "a [[foo]] b";
    expect(findLinkQuery(text, text.length)).toBeNull();
  });

  it("含管道返回 null", () => {
    const text = "[[foo|bar";
    expect(findLinkQuery(text, text.length)).toBeNull();
  });

  it("含 # 返回 null", () => {
    const text = "[[foo#anchor";
    expect(findLinkQuery(text, text.length)).toBeNull();
  });

  it("跨行不触发：上一行的 [[ 不计入", () => {
    const text = "[[foo\nbar";
    expect(findLinkQuery(text, text.length)).toBeNull();
  });

  it("同行最近的 [[ 为准", () => {
    const text = "[[a]] [[foo";
    expect(findLinkQuery(text, text.length)).toEqual({ start: 6, query: "foo" });
  });

  it("代码块行不激活（``` 开头）", () => {
    const text = "```\n[[foo";
    // caret 在第二行，但第一行是 fence；当前行是 "[[foo" 不是 ``` 开头，所以应激活
    // 真正的 fence 行本身是 "```"
    expect(findLinkQuery("```", 3)).toBeNull();
    // 普通行仍激活
    expect(findLinkQuery(text, text.length)).not.toBeNull();
  });

  it("4 空格缩进行不激活", () => {
    const text = "    [[foo";
    expect(findLinkQuery(text, text.length)).toBeNull();
  });
});

describe("buildInsertion", () => {
  it("query 为空 -> [[key]]", () => {
    expect(buildInsertion("a/b", "")).toBe("[[a/b]]");
  });

  it("query 等于 basename（大小写不敏感）-> [[key]]", () => {
    expect(buildInsertion("folder/MyNote", "mynote")).toBe("[[folder/MyNote]]");
    expect(buildInsertion("MyNote", "MyNote")).toBe("[[MyNote]]");
  });

  it("query 非空且不等于 basename -> [[key|query]]", () => {
    expect(buildInsertion("folder/note", "foo")).toBe("[[folder/note|foo]]");
  });
});
