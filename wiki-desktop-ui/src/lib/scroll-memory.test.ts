import { describe, it, expect, beforeEach } from "vitest";
import { readScrollPos, writeScrollPos, __clearScrollMemory } from "./scroll-memory";

describe("scroll-memory", () => {
  beforeEach(() => {
    __clearScrollMemory();
  });

  it("未知 key 默认返回 0", () => {
    expect(readScrollPos("unknown")).toBe(0);
    expect(readScrollPos("")).toBe(0);
  });

  it("读写往返", () => {
    writeScrollPos("note/a", 120);
    expect(readScrollPos("note/a")).toBe(120);
  });

  it("覆盖更新：重复写入取最新值", () => {
    writeScrollPos("note/b", 10);
    writeScrollPos("note/b", 50);
    expect(readScrollPos("note/b")).toBe(50);
  });

  it("空 key 忽略写入与读取", () => {
    writeScrollPos("", 999);
    // 空 key 写入不应影响任何真实 key
    expect(readScrollPos("")).toBe(0);
    // 且不应污染其他 key
    writeScrollPos("real", 10);
    expect(readScrollPos("real")).toBe(10);
  });

  it("多笔记隔离", () => {
    writeScrollPos("note/x", 100);
    writeScrollPos("note/y", 200);
    expect(readScrollPos("note/x")).toBe(100);
    expect(readScrollPos("note/y")).toBe(200);
    // 更新一个不影响另一个
    writeScrollPos("note/x", 300);
    expect(readScrollPos("note/y")).toBe(200);
  });
});
