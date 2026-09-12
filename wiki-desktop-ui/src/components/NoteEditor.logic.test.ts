import { describe, expect, it } from "vitest";
import { findHeadingLine, retryDelayForAttempt } from "./NoteEditor";

describe("retryDelayForAttempt", () => {
  it("5s → 15s → 30s 退避", () => {
    expect(retryDelayForAttempt(0)).toBe(5000);
    expect(retryDelayForAttempt(1)).toBe(15000);
    expect(retryDelayForAttempt(2)).toBe(30000);
  });

  it("超出后封顶 30s", () => {
    expect(retryDelayForAttempt(3)).toBe(30000);
    expect(retryDelayForAttempt(99)).toBe(30000);
  });

  it("负数钳制到首段", () => {
    expect(retryDelayForAttempt(-1)).toBe(5000);
  });
});

describe("findHeadingLine", () => {
  const body = ["# 标题一", "正文", "## 第二章 概览", "更多", "```", "# 不是标题", "```", "### 细节 ##"].join("\n");

  it("精确匹配", () => {
    expect(findHeadingLine(body, "标题一")).toBe(0);
    expect(findHeadingLine(body, "第二章 概览")).toBe(2);
  });

  it("大小写不敏感", () => {
    expect(findHeadingLine("# Hello World", "hello world")).toBe(0);
  });

  it("代码块内标题不算", () => {
    expect(findHeadingLine(body, "不是标题")).toBeNull();
  });

  it("尾部 # 闭合被剥离", () => {
    expect(findHeadingLine(body, "细节")).toBe(7);
  });

  it("找不到 / 空输入返回 null", () => {
    expect(findHeadingLine(body, "不存在")).toBeNull();
    expect(findHeadingLine("", "标题一")).toBeNull();
    expect(findHeadingLine(body, "   ")).toBeNull();
    expect(findHeadingLine("没有标题只有正文", "正文")).toBeNull();
  });
});
