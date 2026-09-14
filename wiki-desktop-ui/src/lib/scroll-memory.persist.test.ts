/**
 * @vitest-environment jsdom
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { SCROLL_MEMORY_STORAGE_KEY, SCROLL_MEMORY_MAX_ENTRIES } from "./scroll-memory";

const KEY = SCROLL_MEMORY_STORAGE_KEY;

/** 拿一份全新的模块实例（模拟进程重启后的首次加载） */
async function freshModule() {
  vi.resetModules();
  return await import("./scroll-memory");
}

function storedEntries(): Record<string, number> {
  return JSON.parse(window.localStorage.getItem(KEY) ?? "{}") as Record<string, number>;
}

describe("scroll-memory 持久化", () => {
  beforeEach(() => {
    window.localStorage.removeItem(KEY);
    vi.resetModules();
  });
  afterEach(() => {
    window.localStorage.removeItem(KEY);
    vi.restoreAllMocks();
  });

  it("读写往返：写入即落盘 JSON", async () => {
    const m = await freshModule();
    m.writeScrollPos("note/a", 120);
    expect(storedEntries()).toMatchObject({ "note/a": 120 });
    expect(m.readScrollPos("note/a")).toBe(120);
  });

  it("重启恢复：新模块实例从 localStorage 懒加载", async () => {
    const m1 = await freshModule();
    m1.writeScrollPos("note/a", 120);
    m1.writeScrollPos("note/b", 340);
    // 模拟重启：丢掉模块缓存后重新导入
    const m2 = await freshModule();
    expect(m2.readScrollPos("note/a")).toBe(120);
    expect(m2.readScrollPos("note/b")).toBe(340);
    expect(m2.readScrollPos("unknown")).toBe(0);
  });

  it("容量上限：超 200 条淘汰最久未用", async () => {
    const m = await freshModule();
    for (let i = 0; i < SCROLL_MEMORY_MAX_ENTRIES + 50; i++) {
      m.writeScrollPos(`note/${i}`, i);
    }
    // 最早写入的 50 条被淘汰
    expect(m.readScrollPos("note/0")).toBe(0);
    expect(m.readScrollPos("note/49")).toBe(0);
    // 最新的保留
    expect(m.readScrollPos(`note/${SCROLL_MEMORY_MAX_ENTRIES + 49}`)).toBe(
      SCROLL_MEMORY_MAX_ENTRIES + 49,
    );
    // 持久化侧同样受限
    expect(Object.keys(storedEntries()).length).toBeLessThanOrEqual(
      SCROLL_MEMORY_MAX_ENTRIES,
    );
  });

  it("LRU：最近读取的不被淘汰", async () => {
    const m = await freshModule();
    for (let i = 0; i < SCROLL_MEMORY_MAX_ENTRIES; i++) {
      m.writeScrollPos(`note/${i}`, i);
    }
    // 读一次 note/0，把它刷成最新
    expect(m.readScrollPos("note/0")).toBe(0);
    // 再写一条触发淘汰：应淘汰 note/1 而非 note/0
    m.writeScrollPos("note/new", 999);
    expect(m.readScrollPos("note/0")).toBe(0);
    expect(m.readScrollPos("note/1")).toBe(0);
    expect(m.readScrollPos("note/new")).toBe(999);
  });

  it("localStorage 抛错时降级为内存行为且不抛错", async () => {
    const m = await freshModule();
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("quota");
    });
    expect(() => m.writeScrollPos("note/a", 120)).not.toThrow();
    expect(m.readScrollPos("note/a")).toBe(120);
    expect(m.readScrollPos("missing")).toBe(0);
  });

  it("坏 JSON / 非法结构降级为空且可恢复写入", async () => {
    window.localStorage.setItem(KEY, "{bad-json");
    const m1 = await freshModule();
    expect(m1.readScrollPos("note/a")).toBe(0);

    window.localStorage.setItem(KEY, JSON.stringify(["not", "an", "object"]));
    const m2 = await freshModule();
    expect(m2.readScrollPos("note/a")).toBe(0);

    // 恢复后可正常写入
    m2.writeScrollPos("note/a", 5);
    expect(m2.readScrollPos("note/a")).toBe(5);
  });

  it("__clearScrollMemory 同时清持久化", async () => {
    const m1 = await freshModule();
    m1.writeScrollPos("note/a", 120);
    expect(window.localStorage.getItem(KEY)).not.toBeNull();
    m1.__clearScrollMemory();
    expect(window.localStorage.getItem(KEY)).toBeNull();
    // 重启后读不到旧数据
    const m2 = await freshModule();
    expect(m2.readScrollPos("note/a")).toBe(0);
  });
});
