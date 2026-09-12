// @vitest-environment node
// toast store 单测：发布订阅、自动消失计时、手动关闭、堆叠上限
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { dismissToast, getToasts, resetToastStore, subscribeToast, toast, TOAST_MAX_ITEMS } from "./toast";

beforeEach(() => {
  vi.useFakeTimers();
  resetToastStore();
});

afterEach(() => {
  resetToastStore();
  vi.useRealTimers();
});

describe("toast store", () => {
  it("success/info/error 推送不同 kind 的 toast", () => {
    toast.success("保存成功");
    toast.error("保存失败");
    toast.info("提示信息");
    const items = getToasts();
    expect(items.map((i) => [i.kind, i.message])).toEqual([
      ["success", "保存成功"],
      ["error", "保存失败"],
      ["info", "提示信息"],
    ]);
    // id 唯一递增
    const ids = items.map((i) => i.id);
    expect(new Set(ids).size).toBe(3);
  });

  it("订阅者收到推送快照，取消订阅后不再收到", () => {
    const seen: string[][] = [];
    const unsub = subscribeToast((items) => seen.push(items.map((i) => i.message)));
    seen.length = 0;
    toast.success("a");
    expect(seen).toEqual([["a"]]);
    toast.info("b");
    expect(seen.at(-1)).toEqual(["a", "b"]);
    unsub();
    toast.error("c");
    expect(seen.at(-1)).toEqual(["a", "b"]);
  });

  it("success/info 3s 自动消失，error 5s 自动消失", () => {
    toast.success("s");
    toast.info("i");
    toast.error("e");
    expect(getToasts()).toHaveLength(3);
    vi.advanceTimersByTime(3000);
    expect(getToasts().map((i) => i.kind)).toEqual(["error"]);
    vi.advanceTimersByTime(2000);
    expect(getToasts()).toHaveLength(0);
  });

  it("dismissToast 手动关闭指定 toast", () => {
    toast.success("a");
    toast.error("b");
    const [first] = getToasts();
    dismissToast(first.id);
    expect(getToasts().map((i) => i.message)).toEqual(["b"]);
    // 关闭不存在的 id 无副作用
    dismissToast(999999);
    expect(getToasts()).toHaveLength(1);
  });

  it("堆叠超过上限时丢弃最早的", () => {
    for (let n = 0; n < TOAST_MAX_ITEMS + 2; n++) toast.info(`m${n}`);
    const items = getToasts();
    expect(items).toHaveLength(TOAST_MAX_ITEMS);
    expect(items.map((i) => i.message)).toEqual(
      Array.from({ length: TOAST_MAX_ITEMS }, (_, k) => `m${k + 2}`),
    );
  });

  it("手动关闭后自动消失计时器不再误删新 toast", () => {
    toast.success("old");
    const [old] = getToasts();
    dismissToast(old.id);
    expect(getToasts()).toHaveLength(0);
    vi.advanceTimersByTime(3000);
    expect(getToasts()).toHaveLength(0);
  });
});
