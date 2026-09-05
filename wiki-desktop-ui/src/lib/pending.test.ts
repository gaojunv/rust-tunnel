import { describe, it, expect } from "vitest";
import { computePendingCount } from "./pending";
import { emptySyncState, hashNote } from "./sync-engine";
import type { SyncState } from "./sync-engine";

describe("computePendingCount", () => {
  it("空笔记空状态返回 0", async () => {
    const state = emptySyncState("kid");
    expect(await computePendingCount([], state)).toBe(0);
    expect(await computePendingCount([], null)).toBe(0);
  });

  it("新 key（无 entry）计为 pending", async () => {
    const state = emptySyncState("kid");
    const notes = [{ key: "a/b", title: "hello", body: "world" }];
    expect(await computePendingCount(notes, state)).toBe(1);
    expect(await computePendingCount(notes, null)).toBe(1);
  });

  it("已同步（hash 一致）不计 pending", async () => {
    const state = emptySyncState("kid");
    const h = await hashNote("hello", "world");
    state.entries["a/b"] = { ref: "a/b", localHash: h, remoteUpdatedAt: "2024-01-01 00:00:00" };
    const notes = [{ key: "a/b", title: "hello", body: "world" }];
    expect(await computePendingCount(notes, state)).toBe(0);
  });

  it("内容变更（hash 不一致）计为 pending", async () => {
    const state = emptySyncState("kid");
    const h = await hashNote("hello", "world");
    state.entries["a/b"] = { ref: "a/b", localHash: h, remoteUpdatedAt: "2024-01-01 00:00:00" };
    const notes = [{ key: "a/b", title: "hello", body: "changed" }];
    expect(await computePendingCount(notes, state)).toBe(1);
  });

  it("跳过冲突副本", async () => {
    const state: SyncState = emptySyncState("kid");
    const notes = [{ key: "a/b.conflict-20240102-030405", title: "t", body: "b" }];
    expect(await computePendingCount(notes, state)).toBe(0);
  });

  it("跳过不兼容 key", async () => {
    const state = emptySyncState("kid");
    const notes = [{ key: "My Note", title: "My Note", body: "hello" }];
    expect(await computePendingCount(notes, state)).toBe(0);
  });

  it("跳过空内容", async () => {
    const state = emptySyncState("kid");
    const notes = [{ key: "a/b", title: "t", body: "   \n\t  " }];
    expect(await computePendingCount(notes, state)).toBe(0);
  });

  it("不兼容但有合法 frontmatter refId 仍计入", async () => {
    const state = emptySyncState("kid");
    const notes = [{ key: "My Note", title: "My Note", body: "hello", refId: "my-note" }];
    expect(await computePendingCount(notes, state)).toBe(1);
    expect(await computePendingCount(notes, null)).toBe(1);
  });

  it("混合：仅统计有效 pending", async () => {
    const state = emptySyncState("kid");
    const h = await hashNote("t1", "b1");
    state.entries["a/b"] = { ref: "a/b", localHash: h, remoteUpdatedAt: "2024-01-01 00:00:00" };
    const notes = [
      { key: "a/b", title: "t1", body: "b1" }, // synced
      { key: "c/d", title: "t2", body: "b2" }, // new → pending
      { key: "e/f", title: "t", body: "   " }, // empty skip
      { key: "My Note", title: "t", body: "x" }, // incompatible skip
    ];
    expect(await computePendingCount(notes, state)).toBe(1);
  });
});
