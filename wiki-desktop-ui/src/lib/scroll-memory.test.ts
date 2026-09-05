import { describe, it, expect, beforeEach } from "vitest";
import { readScrollPos, writeScrollPos, __clearScrollMemory } from "./scroll-memory";

describe("scroll-memory", () => {
  beforeEach(() => {
    __clearScrollMemory();
  });

  it("未知 key 默认返回 0", () => {
    expect(readScrollPos("unknown")).toEqual({ edit: 0, preview: 0 });
    expect(readScrollPos("")).toEqual({ edit: 0, preview: 0 });
  });

  it("读写往返：edit 与 preview 互不干扰", () => {
    writeScrollPos("note/a", "edit", 120);
    expect(readScrollPos("note/a")).toEqual({ edit: 120, preview: 0 });
    writeScrollPos("note/a", "preview", 340);
    expect(readScrollPos("note/a")).toEqual({ edit: 120, preview: 340 });
  });

  it("覆盖更新：重复写入取最新值", () => {
    writeScrollPos("note/b", "edit", 10);
    writeScrollPos("note/b", "edit", 50);
    expect(readScrollPos("note/b").edit).toBe(50);
    writeScrollPos("note/b", "preview", 20);
    writeScrollPos("note/b", "preview", 80);
    expect(readScrollPos("note/b").preview).toBe(80);
  });

  it("空 key 忽略写入与读取", () => {
    writeScrollPos("", "edit", 999);
    writeScrollPos("", "preview", 999);
    // 空 key 写入不应影响任何真实 key
    expect(readScrollPos("")).toEqual({ edit: 0, preview: 0 });
    // 且不应污染其他 key
    writeScrollPos("real", "edit", 10);
    expect(readScrollPos("real")).toEqual({ edit: 10, preview: 0 });
  });

  it("多笔记隔离", () => {
    writeScrollPos("note/x", "edit", 100);
    writeScrollPos("note/y", "edit", 200);
    expect(readScrollPos("note/x").edit).toBe(100);
    expect(readScrollPos("note/y").edit).toBe(200);
    // 更新一个不影响另一个
    writeScrollPos("note/x", "preview", 300);
    expect(readScrollPos("note/y").preview).toBe(0);
  });
});
