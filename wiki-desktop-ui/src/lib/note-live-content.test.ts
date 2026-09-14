import { describe, it, expect, vi, afterEach } from "vitest";
import {
  publishNoteLiveContent,
  subscribeNoteLiveContent,
  getNoteLiveSnapshot,
  clearNoteLiveSnapshot,
  __resetNoteLiveContentForTest,
} from "./note-live-content";

afterEach(() => {
  __resetNoteLiveContentForTest();
});

describe("note-live-content pub/sub", () => {
  it("订阅者能收到发布", () => {
    const received: string[] = [];
    subscribeNoteLiveContent((s) => {
      received.push(s.body);
    });
    publishNoteLiveContent({ noteKey: "a", body: "hello", cursorLine: 0 });
    expect(received).toEqual(["hello"]);
  });

  it("取消订阅后不再收到", () => {
    const fn = vi.fn();
    const unsub = subscribeNoteLiveContent(fn);
    unsub();
    publishNoteLiveContent({ noteKey: "a", body: "x", cursorLine: null });
    expect(fn).not.toHaveBeenCalled();
  });

  it("快照按 noteKey 隔离", () => {
    publishNoteLiveContent({ noteKey: "a", body: "body-a", cursorLine: 1 });
    publishNoteLiveContent({ noteKey: "b", body: "body-b", cursorLine: 2 });
    expect(getNoteLiveSnapshot("a")?.body).toBe("body-a");
    expect(getNoteLiveSnapshot("b")?.cursorLine).toBe(2);
    expect(getNoteLiveSnapshot("c")).toBeNull();
  });

  it("clear 后 getSnapshot 返回 null", () => {
    publishNoteLiveContent({ noteKey: "a", body: "x", cursorLine: 0 });
    clearNoteLiveSnapshot("a");
    expect(getNoteLiveSnapshot("a")).toBeNull();
  });

  it("单个订阅者抛错不影响其他订阅者", () => {
    const fn = vi.fn();
    subscribeNoteLiveContent(() => {
      throw new Error("boom");
    });
    subscribeNoteLiveContent(fn);
    publishNoteLiveContent({ noteKey: "a", body: "x", cursorLine: 0 });
    expect(fn).toHaveBeenCalledTimes(1);
  });
});
