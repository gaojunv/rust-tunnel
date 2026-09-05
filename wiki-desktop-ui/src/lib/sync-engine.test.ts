/**
 * sync-engine 契约测试 —— planSync 全分支 + runSync 核心路径
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  planSync,
  emptySyncState,
  parseRemoteTime,
  conflictCopyKey,
  hashNote,
  type LocalNote,
  type SyncState,
} from "./sync-engine";
import type { RemotePageSummary, ServerApi } from "../api/server";
import { runSync, type SyncIO } from "./sync-engine";

// 辅助：构造本地笔记（hash 用占位，测试按值组装）
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

function remote(ref: string, updated_at: string, extra?: Partial<RemotePageSummary>): RemotePageSummary {
  return { ref, title: ref, locked: false, updated_at, ...extra };
}

describe("parseRemoteTime", () => {
  it("正常解析 UTC", () => {
    // 2024-01-02 03:04:05 UTC
    const t = parseRemoteTime("2024-01-02 03:04:05");
    const expected = Math.floor(Date.parse("2024-01-02T03:04:05Z") / 1000);
    expect(t).toBe(expected);
  });
  it("空串/非法返回 0", () => {
    expect(parseRemoteTime("")).toBe(0);
    expect(parseRemoteTime("not-a-time")).toBe(0);
    expect(parseRemoteTime(null as unknown as string)).toBe(0);
  });
});

describe("conflictCopyKey", () => {
  it("UTC 格式 yyyymmdd-hhmmss", () => {
    // 2024-01-02T03:04:05Z
    const epoch = Math.floor(Date.parse("2024-01-02T03:04:05Z") / 1000);
    expect(conflictCopyKey("a/b", epoch)).toBe("a/b.conflict-20240102-030405");
  });
});

describe("planSync 全分支", () => {
  let state: SyncState;
  beforeEach(() => {
    state = emptySyncState("kid");
  });

  it("首次 upload：本地新、远端无", () => {
    const actions = planSync({
      local: [note({ key: "a/b", contentHash: "h1" })],
      remote: [],
      state,
      propagateDeletes: false,
    });
    expect(actions).toEqual([{ kind: "upload", key: "a/b", ref: "a/b" }]);
  });

  it("首次冲突：本地新、远端已存在 → 按时间判定胜负", () => {
    const remoteTime = parseRemoteTime("2024-01-01 00:00:00");
    const localWin = planSync({
      local: [note({ key: "a/b", modified: remoteTime + 100 })],
      remote: [remote("a/b", "2024-01-01 00:00:00")],
      state,
      propagateDeletes: false,
    });
    expect(localWin[0].kind).toBe("conflict-local-wins");

    const remoteWin = planSync({
      local: [note({ key: "a/b", modified: 0 })],
      remote: [remote("a/b", "2024-01-02 00:00:00")],
      state,
      propagateDeletes: false,
    });
    expect(remoteWin[0].kind).toBe("conflict-remote-wins");
  });

  it("首次冲突相等算本地赢", () => {
    const ts = "2024-01-02 03:04:05";
    const epoch = parseRemoteTime(ts);
    const actions = planSync({
      local: [note({ key: "a/b", modified: epoch })],
      remote: [remote("a/b", ts)],
      state,
      propagateDeletes: false,
    });
    expect(actions[0].kind).toBe("conflict-local-wins");
  });

  it("noop：本地未变、远端未变", () => {
    state.entries["a/b"] = { ref: "a/b", localHash: "h1", remoteUpdatedAt: "2024-01-02 03:04:05" };
    const actions = planSync({
      local: [note({ key: "a/b", contentHash: "h1" })],
      remote: [remote("a/b", "2024-01-02 03:04:05")],
      state,
      propagateDeletes: false,
    });
    expect(actions).toEqual([]);
  });

  it("仅本地变 → upload", () => {
    state.entries["a/b"] = { ref: "a/b", localHash: "h1", remoteUpdatedAt: "2024-01-02 03:04:05" };
    const actions = planSync({
      local: [note({ key: "a/b", contentHash: "h2" })],
      remote: [remote("a/b", "2024-01-02 03:04:05")],
      state,
      propagateDeletes: false,
    });
    expect(actions).toEqual([{ kind: "upload", key: "a/b", ref: "a/b" }]);
  });

  it("仅远端变 → download", () => {
    state.entries["a/b"] = { ref: "a/b", localHash: "h1", remoteUpdatedAt: "2024-01-02 03:04:05" };
    const actions = planSync({
      local: [note({ key: "a/b", contentHash: "h1" })],
      remote: [remote("a/b", "2024-01-03 03:04:05")],
      state,
      propagateDeletes: false,
    });
    expect(actions).toEqual([{ kind: "download", key: "a/b", ref: "a/b" }]);
  });

  it("都变冲突：本地新胜、远端新胜、相等本地胜", () => {
    const base: SyncState = {
      version: 1,
      knowledgeId: "kid",
      entries: { "a/b": { ref: "a/b", localHash: "h1", remoteUpdatedAt: "2024-01-02 03:04:05" } },
      skipped: {},
    };
    // 本地新
    const localWins = planSync({
      local: [note({ key: "a/b", contentHash: "h2", modified: parseRemoteTime("2024-01-04 00:00:00") + 100 })],
      remote: [remote("a/b", "2024-01-04 00:00:00")],
      state: base,
      propagateDeletes: false,
    });
    expect(localWins[0].kind).toBe("conflict-local-wins");

    // 远端新
    const remoteWins = planSync({
      local: [note({ key: "a/b", contentHash: "h2", modified: 0 })],
      remote: [remote("a/b", "2024-01-05 00:00:00")],
      state: base,
      propagateDeletes: false,
    });
    expect(remoteWins[0].kind).toBe("conflict-remote-wins");

    // 相等
    const ts = "2024-01-04 00:00:00";
    const equal = planSync({
      local: [note({ key: "a/b", contentHash: "h2", modified: parseRemoteTime(ts) })],
      remote: [remote("a/b", ts)],
      state: base,
      propagateDeletes: false,
    });
    expect(equal[0].kind).toBe("conflict-local-wins");
  });

  it("远端被删：本地已变 → upload，未变 → restore-remote", () => {
    state.entries["a/b"] = { ref: "a/b", localHash: "h1", remoteUpdatedAt: "2024-01-02 03:04:05" };
    const changed = planSync({
      local: [note({ key: "a/b", contentHash: "h2" })],
      remote: [],
      state,
      propagateDeletes: false,
    });
    expect(changed).toEqual([{ kind: "upload", key: "a/b", ref: "a/b" }]);

    const unchanged = planSync({
      local: [note({ key: "a/b", contentHash: "h1" })],
      remote: [],
      state,
      propagateDeletes: false,
    });
    expect(unchanged).toEqual([{ kind: "restore-remote", key: "a/b", ref: "a/b" }]);
  });

  it("本地被删：propagateDeletes=true 且远端存在 → delete-remote", () => {
    state.entries["a/b"] = { ref: "a/b", localHash: "h1", remoteUpdatedAt: "2024-01-02 03:04:05" };
    const actions = planSync({
      local: [],
      remote: [remote("a/b", "2024-01-02 03:04:05")],
      state,
      propagateDeletes: true,
    });
    expect(actions).toEqual([{ kind: "delete-remote", key: "a/b", ref: "a/b" }]);
  });

  it("本地被删：propagateDeletes=false → drop-state", () => {
    state.entries["a/b"] = { ref: "a/b", localHash: "h1", remoteUpdatedAt: "2024-01-02 03:04:05" };
    const actions = planSync({
      local: [],
      remote: [remote("a/b", "2024-01-02 03:04:05")],
      state,
      propagateDeletes: false,
    });
    expect(actions).toEqual([{ kind: "drop-state", key: "a/b" }]);
  });

  it("本地被删但远端也无 → drop-state（无论开关）", () => {
    state.entries["a/b"] = { ref: "a/b", localHash: "h1", remoteUpdatedAt: "2024-01-02 03:04:05" };
    for (const pd of [true, false]) {
      const actions = planSync({ local: [], remote: [], state, propagateDeletes: pd });
      expect(actions).toEqual([{ kind: "drop-state", key: "a/b" }]);
    }
  });

  it("skip-incompatible：含大写/中文且无合法 frontmatter", () => {
    const actions = planSync({
      local: [note({ key: "My Note", refId: null, body: "hello" })],
      remote: [],
      state,
      propagateDeletes: false,
    });
    expect(actions[0].kind).toBe("skip-incompatible");
  });

  it("skip-incompatible 可被 frontmatter 挽救", () => {
    const actions = planSync({
      local: [note({ key: "My Note", refId: "my-note", body: "hello" })],
      remote: [],
      state,
      propagateDeletes: false,
    });
    expect(actions).toEqual([{ kind: "upload", key: "My Note", ref: "my-note" }]);
  });

  it("skip-empty：body 空白", () => {
    const actions = planSync({
      local: [note({ key: "a/b", body: "   \n\t  ", contentHash: "h1" })],
      remote: [],
      state,
      propagateDeletes: false,
    });
    expect(actions[0].kind).toBe("skip-empty");
  });

  it("skip-conflict-copy：key 最后一段匹配冲突副本", () => {
    const actions = planSync({
      local: [note({ key: "a/b.conflict-20240102-030405", body: "hello" })],
      remote: [],
      state,
      propagateDeletes: false,
    });
    expect(actions[0].kind).toBe("skip-conflict-copy");
  });

  it("冲突副本检测按最后一段，目录含 conflict 前缀不误判", () => {
    const actions = planSync({
      local: [note({ key: "conflict-20240102-030405/a", body: "hello" })],
      remote: [],
      state,
      propagateDeletes: false,
    });
    // 最后一段是 "a"，不匹配冲突后缀，应走正常流程（upload 或 skip-incompatible 取决于 ref 合法性）
    // key "conflict-20240102-030405/a" 归一后为 "conflict-20240102-030405/a" 合法
    expect(actions[0].kind).not.toBe("skip-conflict-copy");
  });
});

// —— runSync ——

function makeFakeIO(overrides?: Partial<SyncIO>): SyncIO & {
  written: Map<string, { title: string; body: string }>;
  remoteCalls: string[];
} {
  const written = new Map<string, { title: string; body: string }>();
  const remoteCalls: string[] = [];
  const remote: SyncIO["remote"] = {
    listAllPages: vi.fn(async () => []),
    getPage: vi.fn(async (ref: string) => ({
      ref,
      title: `title-${ref}`,
      summary: "",
      content: `remote content of ${ref}`,
      locked: false,
      updated_at: "2024-01-03 00:00:00",
    })),
    putPage: vi.fn(async (ref: string, body: { title: string; summary: string; content: string }) => ({
      ref,
      title: body.title,
      summary: body.summary,
      content: body.content,
      locked: false,
      updated_at: "2024-01-03 00:00:00",
    })),
    deletePage: vi.fn(async () => true),
    ...((overrides?.remote as object) ?? {}),
  } as unknown as SyncIO["remote"];

  const io: SyncIO & { written: typeof written; remoteCalls: typeof remoteCalls } = {
    local: {
      writeNote: vi.fn(async (key: string, title: string, body: string) => {
        written.set(key, { title, body });
        remoteCalls.push(`write:${key}`);
        return { modified: 9999 };
      }),
    },
    remote,
    now: () => Math.floor(Date.parse("2024-01-04T00:00:00Z") / 1000),
    written,
    remoteCalls,
    ...overrides,
  } as unknown as SyncIO & { written: typeof written; remoteCalls: typeof remoteCalls };

  // 若 overrides 提供了 custom remote/local，保留 written 引用
  if (overrides?.local) (io as unknown as Record<string, unknown>)["local"] = overrides.local;
  if (overrides?.remote) (io as unknown as Record<string, unknown>)["remote"] = overrides.remote;
  // 确保 written 仍可访问
  (io as unknown as Record<string, unknown>)["written"] = written;
  (io as unknown as Record<string, unknown>)["remoteCalls"] = remoteCalls;
  return io;
}

describe("runSync", () => {
  it("upload 成功更新 state", async () => {
    const state = emptySyncState("kid");
    const n = note({ key: "a/b", title: "hello", body: "world", contentHash: "h1" });
    const localByKey = new Map([["a/b", n]]);
    const io = makeFakeIO();
    const report = await runSync([{ kind: "upload", key: "a/b", ref: "a/b" }], {
      localByKey,
      io,
      state,
    });
    expect(report.uploaded).toBe(1);
    expect(report.errors).toBe(0);
    expect(state.entries["a/b"]).toEqual({
      ref: "a/b",
      localHash: "h1",
      remoteUpdatedAt: "2024-01-03 00:00:00",
    });
  });

  it("download 写入本地并更新 hash", async () => {
    const state = emptySyncState("kid");
    const n = note({ key: "a/b", title: "old", body: "old body", contentHash: "old" });
    const localByKey = new Map([["a/b", n]]);
    const io = makeFakeIO();
    const report = await runSync([{ kind: "download", key: "a/b", ref: "a/b" }], {
      localByKey,
      io,
      state,
    });
    expect(report.downloaded).toBe(1);
    expect(io.written.has("a/b")).toBe(true);
    const expectedHash = await hashNote("title-a/b", "remote content of a/b");
    expect(state.entries["a/b"].localHash).toBe(expectedHash);
    expect(state.entries["a/b"].remoteUpdatedAt).toBe("2024-01-03 00:00:00");
  });

  it("delete-remote 与 drop-state 清理 entries", async () => {
    const state: SyncState = {
      version: 1,
      knowledgeId: "kid",
      entries: {
        "a/b": { ref: "a/b", localHash: "h1", remoteUpdatedAt: "t1" },
        "c/d": { ref: "c/d", localHash: "h2", remoteUpdatedAt: "t2" },
      },
      skipped: {},
    };
    const io = makeFakeIO();
    const report = await runSync(
      [
        { kind: "delete-remote", key: "a/b", ref: "a/b" },
        { kind: "drop-state", key: "c/d" },
      ],
      { localByKey: new Map(), io, state },
    );
    expect(report.deletedRemote).toBe(1);
    expect(report.skipped).toBe(1);
    expect(state.entries["a/b"]).toBeUndefined();
    expect(state.entries["c/d"]).toBeUndefined();
  });

  it("单条失败不中止后续", async () => {
    const state = emptySyncState("kid");
    const n1 = note({ key: "a/b", title: "t", body: "b1", contentHash: "h1" });
    const n2 = note({ key: "c/d", title: "t2", body: "b2", contentHash: "h2" });
    const localByKey = new Map([
      ["a/b", n1],
      ["c/d", n2],
    ]);
    const failingRemote = {
      listAllPages: vi.fn(async () => []),
      getPage: vi.fn(async () => null),
      putPage: vi.fn(async (ref: string) => {
        if (ref === "a/b") throw new Error("network fail");
        return {
          ref,
          title: "t2",
          summary: "",
          content: "b2",
          locked: false,
          updated_at: "2024-01-03 00:00:00",
        };
      }),
      deletePage: vi.fn(async () => true),
    } as unknown as ServerApi;
    const written = new Map<string, { title: string; body: string }>();
    const io: SyncIO = {
      local: {
        writeNote: async (k: string, t: string, b: string) => {
          written.set(k, { title: t, body: b });
          return { modified: 0 };
        },
      },
      remote: failingRemote,
      now: () => 0,
    };
    const report = await runSync(
      [
        { kind: "upload", key: "a/b", ref: "a/b" },
        { kind: "upload", key: "c/d", ref: "c/d" },
      ],
      { localByKey, io, state },
    );
    expect(report.errors).toBe(1);
    expect(report.uploaded).toBe(1);
    expect(report.items[0].ok).toBe(false);
    expect(report.items[1].ok).toBe(true);
    expect(state.entries["c/d"]).toBeDefined();
    expect(state.entries["a/b"]).toBeUndefined();
  });

  it("locked 检测：远端返回 content 不一致则标记 locked-skipped 且不更新 entry", async () => {
    const state = emptySyncState("kid");
    const n = note({ key: "a/b", title: "t", body: "local body", contentHash: "h1" });
    const localByKey = new Map([["a/b", n]]);
    const fakeRemote = {
      listAllPages: vi.fn(async () => []),
      getPage: vi.fn(async () => null),
      putPage: vi.fn(async () => ({
        ref: "a/b",
        title: "t",
        summary: "",
        content: "different content (locked)",
        locked: true,
        updated_at: "2024-01-03 00:00:00",
      })),
      deletePage: vi.fn(async () => true),
    } as unknown as ServerApi;
    const io: SyncIO = {
      local: { writeNote: async () => ({ modified: 0 }) },
      remote: fakeRemote,
      now: () => 0,
    };
    const report = await runSync([{ kind: "upload", key: "a/b", ref: "a/b" }], {
      localByKey,
      io,
      state,
    });
    expect(report.errors).toBe(1);
    expect(report.items[0].detail).toBe("locked-skipped");
    expect(state.entries["a/b"]).toBeUndefined();
  });

  it("冲突副本内容正确：conflict-local-wins 保存远端副本，conflict-remote-wins 保存本地副本", async () => {
    const nowEpoch = Math.floor(Date.parse("2024-01-04T12:00:00Z") / 1000);
    const n = note({ key: "a/b", title: "local title", body: "local body", contentHash: "h1" });
    const state1 = emptySyncState("kid");
    const written1 = new Map<string, { title: string; body: string }>();
    const remote1 = {
      listAllPages: vi.fn(async () => []),
      getPage: vi.fn(async () => ({
        ref: "a/b",
        title: "remote title",
        summary: "",
        content: "remote body",
        locked: false,
        updated_at: "2024-01-02 00:00:00",
      })),
      putPage: vi.fn(async (ref: string, body: { title: string; summary: string; content: string }) => ({
        ref,
        title: body.title,
        summary: body.summary,
        content: body.content,
        locked: false,
        updated_at: "2024-01-03 00:00:00",
      })),
      deletePage: vi.fn(async () => true),
    } as unknown as ServerApi;
    const io1: SyncIO = {
      local: {
        writeNote: async (k: string, t: string, b: string) => {
          written1.set(k, { title: t, body: b });
          return { modified: 0 };
        },
      },
      remote: remote1,
      now: () => nowEpoch,
    };
    const r1 = await runSync([{ kind: "conflict-local-wins", key: "a/b", ref: "a/b" }], {
      localByKey: new Map([["a/b", n]]),
      io: io1,
      state: state1,
    });
    expect(r1.conflicts).toBe(1);
    const copyKey1 = conflictCopyKey("a/b", nowEpoch);
    expect(written1.get(copyKey1)?.body).toBe("remote body");
    expect(written1.get(copyKey1)?.title).toBe("a/b");

    const state2 = emptySyncState("kid");
    const written2 = new Map<string, { title: string; body: string }>();
    const remote2 = {
      listAllPages: vi.fn(async () => []),
      getPage: vi.fn(async () => ({
        ref: "a/b",
        title: "remote title2",
        summary: "",
        content: "remote body2",
        locked: false,
        updated_at: "2024-01-02 00:00:00",
      })),
      putPage: vi.fn(async () => ({
        ref: "a/b",
        title: "remote title2",
        summary: "",
        content: "remote body2",
        locked: false,
        updated_at: "2024-01-03 00:00:00",
      })),
      deletePage: vi.fn(async () => true),
    } as unknown as ServerApi;
    const io2: SyncIO = {
      local: {
        writeNote: async (k: string, t: string, b: string) => {
          written2.set(k, { title: t, body: b });
          return { modified: 0 };
        },
      },
      remote: remote2,
      now: () => nowEpoch,
    };
    const r2 = await runSync([{ kind: "conflict-remote-wins", key: "a/b", ref: "a/b" }], {
      localByKey: new Map([["a/b", n]]),
      io: io2,
      state: state2,
    });
    expect(r2.conflicts).toBe(1);
    const copyKey2 = conflictCopyKey("a/b", nowEpoch);
    expect(written2.get(copyKey2)?.body).toBe("local body");
    expect(written2.get(copyKey2)?.title).toBe("a/b");
    // conflict-remote-wins 之后还会写回远端内容到原 key
    expect(written2.get("a/b")?.body).toBe("remote body2");
  });

  it("title 超 64 字符截断", async () => {
    const state = emptySyncState("kid");
    const longTitle = "标题".repeat(40); // 80 字符
    const n = note({ key: "a/b", title: longTitle, body: "hello", contentHash: "h1" });
    let putTitle = "";
    const fakeRemote = {
      listAllPages: vi.fn(async () => []),
      getPage: vi.fn(async () => null),
      putPage: vi.fn(async (_ref: string, body: { title: string; summary: string; content: string }) => {
        putTitle = body.title;
        return {
          ref: "a/b",
          title: body.title,
          summary: body.summary,
          content: body.content,
          locked: false,
          updated_at: "2024-01-03 00:00:00",
        };
      }),
      deletePage: vi.fn(async () => true),
    } as unknown as ServerApi;
    const io: SyncIO = {
      local: { writeNote: async () => ({ modified: 0 }) },
      remote: fakeRemote,
      now: () => 0,
    };
    await runSync([{ kind: "upload", key: "a/b", ref: "a/b" }], {
      localByKey: new Map([["a/b", n]]),
      io,
      state,
    });
    expect([...putTitle].length).toBe(64);
  });

  it("hashNote 一致性", async () => {
    const h1 = await hashNote("title", "body");
    const h2 = await hashNote("title", "body");
    const h3 = await hashNote("title", "other");
    expect(h1).toBe(h2);
    expect(h1).not.toBe(h3);
    expect(h1).toMatch(/^[0-9a-f]{64}$/);
  });
});
