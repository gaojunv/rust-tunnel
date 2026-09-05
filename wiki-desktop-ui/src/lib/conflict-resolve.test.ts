import { describe, it, expect, vi } from "vitest";
import { emptySyncState, hashNote, conflictCopyKey } from "./sync-engine";
import { conflictsFromReport, applyResolution, type ConflictIO, type PendingConflict } from "./conflict-resolve";
import type { SyncReport } from "./sync-engine";

function pending(): PendingConflict {
  return { key: "a/b", ref: "a/b", localModified: 1000, remoteUpdatedAt: "2024-01-03 00:00:00" };
}

function fakeIO(overrides?: Partial<ConflictIO>): ConflictIO & { writes: Array<{ key: string; body: string; title?: string }>; puts: Array<{ ref: string; body: { title: string; summary: string; content: string } }> } {
  const writes: Array<{ key: string; body: string; title?: string }> = [];
  const puts: Array<{ ref: string; body: { title: string; summary: string; content: string } }> = [];
  const io: ConflictIO & { writes: typeof writes; puts: typeof puts } = {
    writeNote: vi.fn(async (key: string, body: string, title?: string) => {
      writes.push({ key, body, title });
      return {};
    }),
    putPage: vi.fn(async (ref: string, body: { title: string; summary: string; content: string }) => {
      puts.push({ ref, body });
      return { updated_at: "2024-01-04 00:00:00", content: body.content };
    }),
    now: () => Math.floor(Date.parse("2024-01-04T12:00:00Z") / 1000),
    writes,
    puts,
    ...overrides,
  } as unknown as ConflictIO & { writes: typeof writes; puts: typeof puts };
  if (overrides?.writeNote) (io as unknown as Record<string, unknown>).writeNote = overrides.writeNote;
  if (overrides?.putPage) (io as unknown as Record<string, unknown>).putPage = overrides.putPage;
  if (overrides?.now) (io as unknown as Record<string, unknown>).now = overrides.now;
  return io;
}

describe("conflictsFromReport", () => {
  it("filters conflict-pending items", () => {
    const report: SyncReport = {
      items: [
        { action: { kind: "upload", key: "a", ref: "a" }, ok: true },
        { action: { kind: "conflict-pending", key: "a/b", ref: "a/b", localModified: 1, remoteUpdatedAt: "t" }, ok: true },
        { action: { kind: "conflict-pending", key: "c/d", ref: "c/d", localModified: 2, remoteUpdatedAt: "t2" }, ok: true },
      ],
      uploaded: 1, downloaded: 0, conflicts: 2, restored: 0, deletedRemote: 0, skipped: 0, errors: 0,
    };
    const pcs = conflictsFromReport(report);
    expect(pcs).toEqual([
      { key: "a/b", ref: "a/b", localModified: 1, remoteUpdatedAt: "t" },
      { key: "c/d", ref: "c/d", localModified: 2, remoteUpdatedAt: "t2" },
    ]);
  });
});

describe("applyResolution", () => {
  it("local: putPage then state entry", async () => {
    const state = emptySyncState("kid");
    const io = fakeIO();
    await applyResolution(io, state, pending(), { title: "t1", body: "local body" }, { title: "rt", content: "remote body", updated_at: "2024-01-03 00:00:00" }, "local");
    expect(io.puts.length).toBe(1);
    expect(io.puts[0].ref).toBe("a/b");
    expect(io.puts[0].body.content).toBe("local body");
    expect(io.writes.length).toBe(0);
    expect(state.entries["a/b"].remoteUpdatedAt).toBe("2024-01-04 00:00:00");
    expect(state.entries["a/b"].localHash).toBe(await hashNote("t1", "local body"));
  });

  it("local: locked throws", async () => {
    const state = emptySyncState("kid");
    const io = fakeIO({ putPage: async () => ({ updated_at: "t", content: "different" }) });
    await expect(applyResolution(io, state, pending(), { title: "t1", body: "local body" }, { title: "rt", content: "rb", updated_at: "t" }, "local")).rejects.toThrow("远端页面已锁定");
    expect(state.entries["a/b"]).toBeUndefined();
  });

  it("remote: writeNote then state entry", async () => {
    const state = emptySyncState("kid");
    const io = fakeIO();
    await applyResolution(io, state, pending(), { title: "t1", body: "local body" }, { title: "rt", content: "remote body", updated_at: "2024-01-03 00:00:00" }, "remote");
    expect(io.writes.length).toBe(1);
    expect(io.writes[0]).toMatchObject({ key: "a/b", body: "remote body", title: "rt" });
    expect(io.puts.length).toBe(0);
    expect(state.entries["a/b"].localHash).toBe(await hashNote("rt", "remote body"));
    expect(state.entries["a/b"].remoteUpdatedAt).toBe("2024-01-03 00:00:00");
  });

  it("both: write conflict copy then local upload", async () => {
    const state = emptySyncState("kid");
    const now = Math.floor(Date.parse("2024-01-04T12:00:00Z") / 1000);
    const io = fakeIO({ now: () => now });
    await applyResolution(io, state, pending(), { title: "t1", body: "local body" }, { title: "rt", content: "remote body", updated_at: "2024-01-03 00:00:00" }, "both");
    expect(io.writes.length).toBe(1);
    expect(io.writes[0].key).toBe(conflictCopyKey("a/b", now));
    expect(io.writes[0].body).toBe("remote body");
    expect(io.puts.length).toBe(1);
    expect(state.entries["a/b"].localHash).toBe(await hashNote("t1", "local body"));
  });

  it("both: locked after copy throws", async () => {
    const state = emptySyncState("kid");
    const io = fakeIO({ putPage: async () => ({ updated_at: "t", content: "diff" }) });
    await expect(applyResolution(io, state, pending(), { title: "t1", body: "local body" }, { title: "rt", content: "rb", updated_at: "t" }, "both")).rejects.toThrow("远端页面已锁定");
  });

  it("merged: writeNote + putPage then entry with merged hash", async () => {
    const state = emptySyncState("kid");
    const io = fakeIO();
    await applyResolution(io, state, pending(), { title: "t1", body: "local" }, { title: "rt", content: "remote", updated_at: "2024-01-03 00:00:00" }, { merged: { title: "mt", body: "merged body" } });
    expect(io.writes.length).toBe(1);
    expect(io.writes[0]).toMatchObject({ key: "a/b", body: "merged body", title: "mt" });
    expect(io.puts.length).toBe(1);
    expect(io.puts[0].body.content).toBe("merged body");
    expect(state.entries["a/b"].localHash).toBe(await hashNote("mt", "merged body"));
    expect(state.entries["a/b"].remoteUpdatedAt).toBe("2024-01-04 00:00:00");
  });

  it("merged: locked throws", async () => {
    const state = emptySyncState("kid");
    const io = fakeIO({ putPage: async () => ({ updated_at: "t", content: "x" }) });
    await expect(applyResolution(io, state, pending(), { title: "t1", body: "a" }, { title: "rt", content: "r", updated_at: "t" }, { merged: { title: "mt", body: "merged" } })).rejects.toThrow("远端页面已锁定");
  });

  it("truncates title >64 chars", async () => {
    const state = emptySyncState("kid");
    const io = fakeIO();
    const long = "a".repeat(100);
    await applyResolution(io, state, pending(), { title: long, body: "b" }, { title: "rt", content: "r", updated_at: "t" }, "local");
    expect([...io.puts[0].body.title].length).toBe(64);
  });
});
