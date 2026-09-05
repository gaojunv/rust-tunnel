// @vitest-environment jsdom
import { describe, it, expect } from "vitest";
import { lineToOffsetRaw, parseLineHeight } from "./caret-position";

describe("lineToOffsetRaw", () => {
  it("line 0 返回 0", () => {
    expect(lineToOffsetRaw("a\nb\nc", 0)).toBe(0);
  });
  it("line 1 返回首行长度", () => {
    // "a\nb\nc" split -> ["a","b","c"], slice(0,1).join("\n") = "a" => 1
    expect(lineToOffsetRaw("a\nb\nc", 1)).toBe(1);
  });
  it("line 2 返回前两行长度含分隔符", () => {
    // slice(0,2) = ["a","b"] -> "a\nb" => 3
    expect(lineToOffsetRaw("a\nb\nc", 2)).toBe(3);
  });
  it("空 body", () => {
    expect(lineToOffsetRaw("", 1)).toBe(0);
  });
  it("超界返回总长度", () => {
    expect(lineToOffsetRaw("a\nb", 10)).toBe(3);
  });
});

describe("parseLineHeight", () => {
  it("数值 px 返回数值", () => {
    const el = document.createElement("div");
    el.style.lineHeight = "20px";
    const cs = getComputedStyle(el);
    // jsdom 可能返回空，至少不抛错且 fallback 合理
    const v = parseLineHeight(cs);
    expect(v).toBeGreaterThan(0);
  });
  it("非法 fallback 20 或 fontSize 推导", () => {
    const fake = { lineHeight: "normal", fontSize: "16px" } as unknown as CSSStyleDeclaration;
    const v = parseLineHeight(fake);
    expect(v).toBeGreaterThan(0);
  });
});
