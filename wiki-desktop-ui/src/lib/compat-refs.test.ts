import { describe, it, expect, vi } from "vitest";
import { ensureCompatibleRefs } from "./compat-refs";
import type { LocalNote } from "./sync-engine";

function note(overrides: Partial<LocalNote> & { key: string }): LocalNote {
  return {
    refId: null,
    title: overrides.title ?? overrides.key,
    body: overrides.body ?? `body of ${overrides.key}`,
    modified: overrides.modified ?? 1000,
    contentHash: overrides.contentHash ?? `hash-${overrides.key}`,
    ...overrides,
  };
}

describe("ensureCompatibleRefs", () => {
  it("兼容 key 不调用 setRef", async () => {
    const notes = [note({ key: "a/b", refId: null, body: "hello" })];
    const setRef = vi.fn(async () => ({ modified: 9999 }));
    const n = await ensureCompatibleRefs(notes, setRef);
    expect(n).toBe(0);
    expect(setRef).not.toHaveBeenCalled();
  });

  it("空 body 不调用", async () => {
    const notes = [note({ key: "中文", refId: null, body: "   " })];
    const setRef = vi.fn(async () => ({ modified: 9999 }));
    const n = await ensureCompatibleRefs(notes, setRef);
    expect(n).toBe(0);
    expect(setRef).not.toHaveBeenCalled();
  });

  it("冲突副本不调用", async () => {
    const notes = [note({ key: "a/b.conflict-20240102-030405", refId: null, body: "hello" })];
    const setRef = vi.fn(async () => ({ modified: 9999 }));
    const n = await ensureCompatibleRefs(notes, setRef);
    expect(n).toBe(0);
    expect(setRef).not.toHaveBeenCalled();
  });

  it("refId 非法（如 Bad Ref）不调用", async () => {
    const notes = [note({ key: "My Note", refId: "Bad Ref", body: "hello" })];
    const setRef = vi.fn(async () => ({ modified: 9999 }));
    const n = await ensureCompatibleRefs(notes, setRef);
    expect(n).toBe(0);
    expect(setRef).not.toHaveBeenCalled();
  });

  it("refId 空且 key 不兼容时调用 setRef 且更新 note", async () => {
    const notes = [note({ key: "数据库", refId: null, body: "hello", modified: 1000 })];
    // eslint-disable-next-line @typescript-eslint/no-unused-vars
    const setRef = vi.fn(async (_key: string, _ref: string) => ({ modified: 12345 }));
    const n = await ensureCompatibleRefs(notes, setRef);
    expect(n).toBe(1);
    expect(setRef).toHaveBeenCalledTimes(1);
    const ref = setRef.mock.calls[0][1];
    expect(ref).toMatch(/^n-[0-9a-f]{12}$/);
    expect(notes[0].refId).toBe(ref);
    expect(notes[0].modified).toBe(12345);
  });

  it("setRef 抛错时不中断后续", async () => {
    const notes = [
      note({ key: "中文1", refId: null, body: "hello" }),
      note({ key: "中文2", refId: null, body: "hello" }),
    ];
    const setRef = vi.fn(async (key: string) => {
      if (key === "中文1") throw new Error("fail");
      return { modified: 9999 };
    });
    const n = await ensureCompatibleRefs(notes, setRef);
    expect(n).toBe(1);
    expect(setRef).toHaveBeenCalledTimes(2);
    expect(notes[0].refId).toBeNull();
    expect(notes[1].refId).toMatch(/^n-[0-9a-f]{12}$/);
  });
});
