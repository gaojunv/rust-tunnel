import { describe, expect, it } from "vitest";
import { resolveQuickCreate } from "./quick-create";

describe("resolveQuickCreate", () => {
  it("空 query 时隐藏", () => {
    expect(resolveQuickCreate("", [], true)).toEqual({ kind: "hidden" });
    expect(resolveQuickCreate("   ", [], true)).toEqual({ kind: "hidden" });
  });

  it("未接入 onCreate 时隐藏", () => {
    expect(resolveQuickCreate("new-note", [], false)).toEqual({ kind: "hidden" });
  });

  it("非法字符返回 invalid 并带中文提示", () => {
    const r = resolveQuickCreate("a:b", [], true);
    expect(r.kind).toBe("invalid");
    if (r.kind === "invalid") expect(r.error).toContain(":");
  });

  it("前导 /、尾随 /、.. 段均 invalid", () => {
    expect(resolveQuickCreate("/a", [], true).kind).toBe("invalid");
    expect(resolveQuickCreate("a/", [], true).kind).toBe("invalid");
    expect(resolveQuickCreate("a/../b", [], true).kind).toBe("invalid");
  });

  it("已存在同名返回 duplicate", () => {
    expect(resolveQuickCreate("exist", ["exist", "other"], true)).toEqual({
      kind: "duplicate",
    });
  });

  it("合法新名返回 ready（trim 后作为 key）", () => {
    expect(resolveQuickCreate("  新笔记/x  ", ["other"], true)).toEqual({
      kind: "ready",
      key: "新笔记/x",
    });
  });

  it("key 比对大小写敏感", () => {
    expect(resolveQuickCreate("Note", ["note"], true)).toEqual({
      kind: "ready",
      key: "Note",
    });
  });
});
