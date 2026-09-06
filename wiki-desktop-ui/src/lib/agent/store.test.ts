/**
 * @vitest-environment jsdom
 */
import { describe, it, expect, beforeEach, vi } from "vitest";
import {
  vaultHash,
  listThreads,
  addThread,
  updateThreadTitle,
  archiveThread,
  removeThread,
  __clearStoreForTest,
} from "./store";

describe("vaultHash", () => {
  it("同一输入稳定，空串也有值", () => {
    expect(vaultHash("/a/b")).toBe(vaultHash("/a/b"));
    expect(vaultHash("")).toMatch(/^[0-9a-f]{8}$/);
  });

  it("不同输入不同 hash", () => {
    expect(vaultHash("/a")).not.toBe(vaultHash("/b"));
  });
});

describe("store CRUD", () => {
  beforeEach(() => {
    __clearStoreForTest();
  });

  it("往返：add/list", () => {
    addThread("/vault-a", { threadId: "t1", title: "hello", createdAt: 1 });
    addThread("/vault-a", { threadId: "t2", title: "world", createdAt: 2 });
    const list = listThreads("/vault-a");
    expect(list.map((r) => r.threadId)).toEqual(["t2", "t1"]);
  });

  it("按 vault 分桶隔离", () => {
    addThread("/vault-a", { threadId: "t1", title: "a", createdAt: 1 });
    addThread("/vault-b", { threadId: "t1", title: "b", createdAt: 1 });
    expect(listThreads("/vault-a")[0].title).toBe("a");
    expect(listThreads("/vault-b")[0].title).toBe("b");
  });

  it("同 threadId 去重更新", () => {
    addThread("/vault-a", { threadId: "t1", title: "old", createdAt: 1 });
    addThread("/vault-a", { threadId: "t1", title: "new", createdAt: 2 });
    const list = listThreads("/vault-a");
    expect(list.length).toBe(1);
    expect(list[0].title).toBe("new");
  });

  it("updateTitle/archive/remove", () => {
    addThread("/vault-a", { threadId: "t1", title: "old", createdAt: 1 });
    updateThreadTitle("/vault-a", "t1", "renamed");
    expect(listThreads("/vault-a")[0].title).toBe("renamed");
    archiveThread("/vault-a", "t1");
    expect(listThreads("/vault-a")[0].archived).toBe(true);
    removeThread("/vault-a", "t1");
    expect(listThreads("/vault-a").length).toBe(0);
  });

  it("localStorage 异常容错（隐私模式）", () => {
    const getSpy = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("denied");
    });
    expect(listThreads("/vault-a")).toEqual([]);
    getSpy.mockRestore();

    const setSpy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("denied");
    });
    // 不应抛错
    expect(() => addThread("/vault-a", { threadId: "t1", title: "t", createdAt: 1 })).not.toThrow();
    setSpy.mockRestore();
  });

  it("非法 JSON 或非对象容错", () => {
    localStorage.setItem("wiki.agent.threads.v1", "not-json");
    expect(listThreads("/vault-a")).toEqual([]);
    localStorage.setItem("wiki.agent.threads.v1", "[]");
    expect(listThreads("/vault-a")).toEqual([]);
  });
});
