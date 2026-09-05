/**
 * 双向同步核心 —— 纯逻辑 + 依赖注入
 * 方向判定严格按任务说明伪码实现
 */

import { toRemoteRef } from "./ref-id";
import type { RemotePageSummary, ServerApi } from "../api/server";

/** 冲突副本检测：最后一段匹配 /\.conflict-\d{8}-\d{6}$/（模块级导出） */
export function isConflictCopyKey(key: string): boolean {
  const lastSeg = key.split("/").pop() ?? key;
  return /\.conflict-\d{8}-\d{6}$/.test(lastSeg);
}

/** 本地笔记（调用方负责读取并计算 contentHash） */
export interface LocalNote {
  key: string;
  refId: string | null;
  title: string;
  body: string;
  modified: number;
  contentHash: string;
}

/** 同步状态条目 */
export interface SyncStateEntry {
  ref: string;
  localHash: string;
  remoteUpdatedAt: string;
}

/** 同步状态全量 */
export interface SyncState {
  version: 1;
  knowledgeId: string;
  entries: Record<string, SyncStateEntry>;
  skipped: Record<string, string>;
}

/**
 * 创建空同步状态
 */
export function emptySyncState(knowledgeId: string): SyncState {
  return { version: 1, knowledgeId, entries: {}, skipped: {} };
}

/**
 * 解析远端时间字符串 "YYYY-MM-DD HH:MM:SS" 为 epoch 秒（按 UTC）
 * 容错返回 0
 */
export function parseRemoteTime(s: string): number {
  try {
    if (!s || typeof s !== "string") return 0;
    const iso = s.trim().replace(" ", "T") + "Z";
    const ms = Date.parse(iso);
    if (Number.isNaN(ms)) return 0;
    return Math.floor(ms / 1000);
  } catch {
    return 0;
  }
}

/**
 * 生成冲突副本 key：`<key>.conflict-<yyyymmdd-hhmmss>`（UTC）
 */
export function conflictCopyKey(key: string, now: number): string {
  const d = new Date(now * 1000);
  const pad = (n: number, len = 2) => String(n).padStart(len, "0");
  const y = d.getUTCFullYear();
  const m = pad(d.getUTCMonth() + 1);
  const day = pad(d.getUTCDate());
  const hh = pad(d.getUTCHours());
  const mm = pad(d.getUTCMinutes());
  const ss = pad(d.getUTCSeconds());
  return `${key}.conflict-${y}${m}${day}-${hh}${mm}${ss}`;
}

/**
 * 计算笔记 hash（title + body 的 SHA-256 hex）
 */
export async function hashNote(title: string, body: string): Promise<string> {
  const data = new TextEncoder().encode(`${title}\n${body}`);
  const buf = await crypto.subtle.digest("SHA-256", data);
  const arr = new Uint8Array(buf);
  return Array.from(arr)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

// —— Action 定义 ——

export type Action =
  | { kind: "upload"; key: string; ref: string }
  | { kind: "download"; key: string; ref: string }
  | { kind: "conflict-local-wins"; key: string; ref: string }
  | { kind: "conflict-remote-wins"; key: string; ref: string }
  | { kind: "restore-remote"; key: string; ref: string }
  | { kind: "delete-remote"; key: string; ref: string }
  | { kind: "drop-state"; key: string }
  | { kind: "skip-incompatible"; key: string; reason: string }
  | { kind: "skip-empty"; key: string }
  | { kind: "skip-conflict-copy"; key: string };

/**
 * 规划同步动作（纯函数）
 */
export function planSync(input: {
  local: LocalNote[];
  remote: RemotePageSummary[];
  state: SyncState;
  propagateDeletes: boolean;
}): Action[] {
  const { local, remote, state, propagateDeletes } = input;
  const remoteByRef = new Map<string, RemotePageSummary>();
  for (const r of remote) remoteByRef.set(r.ref, r);

  const seenLocal = new Set<string>();
  const actions: Action[] = [];

  for (const note of local) {
    // 1. 冲突副本直接跳过（防无限传播）
    if (isConflictCopyKey(note.key)) {
      actions.push({ kind: "skip-conflict-copy", key: note.key });
      // 冲突副本本身仍视为已见，避免被误删判定
      seenLocal.add(note.key);
      continue;
    }

    const ref = toRemoteRef(note.key, note.refId);
    if (ref == null) {
      actions.push({
        kind: "skip-incompatible",
        key: note.key,
        reason: `key "${note.key}" 含大写/中文或非法字符且 frontmatter 未提供合法 ref`,
      });
      // 不兼容的笔记仍视为已见，避免误删远端
      seenLocal.add(note.key);
      continue;
    }

    seenLocal.add(note.key);

    if (note.body.trim() === "") {
      actions.push({ kind: "skip-empty", key: note.key });
      continue;
    }

    const e = state.entries[note.key] ?? null;
    const r = remoteByRef.get(ref) ?? null;

    if (e == null) {
      // 首次见面
      if (r == null) {
        actions.push({ kind: "upload", key: note.key, ref });
      } else {
        const localWins = note.modified >= parseRemoteTime(r.updated_at);
        actions.push({
          kind: localWins ? "conflict-local-wins" : "conflict-remote-wins",
          key: note.key,
          ref,
        });
      }
    } else {
      const localChanged = note.contentHash !== e.localHash;
      if (r == null) {
        // 远端被删
        if (localChanged) {
          actions.push({ kind: "upload", key: note.key, ref });
        } else {
          actions.push({ kind: "restore-remote", key: note.key, ref });
        }
      } else {
        const remoteChanged = r.updated_at !== e.remoteUpdatedAt;
        if (!localChanged && !remoteChanged) {
          // noop
          continue;
        }
        if (localChanged && !remoteChanged) {
          actions.push({ kind: "upload", key: note.key, ref });
        } else if (!localChanged && remoteChanged) {
          actions.push({ kind: "download", key: note.key, ref });
        } else {
          // 都变 → 冲突，新的赢（相等算本地赢）
          const localWins = note.modified >= parseRemoteTime(r.updated_at);
          actions.push({
            kind: localWins ? "conflict-local-wins" : "conflict-remote-wins",
            key: note.key,
            ref,
          });
        }
      }
    }
  }

  // 本地已删
  for (const key of Object.keys(state.entries)) {
    if (seenLocal.has(key)) continue;
    const e = state.entries[key];
    if (propagateDeletes && remoteByRef.has(e.ref)) {
      actions.push({ kind: "delete-remote", key, ref: e.ref });
    } else {
      actions.push({ kind: "drop-state", key });
    }
  }

  return actions;
}

// —— 执行器 ——

export interface SyncItemResult {
  action: Action;
  ok: boolean;
  detail?: string;
}

export interface SyncReport {
  items: SyncItemResult[];
  uploaded: number;
  downloaded: number;
  conflicts: number;
  restored: number;
  deletedRemote: number;
  skipped: number;
  errors: number;
}

export interface SyncIO {
  local: {
    writeNote(key: string, title: string, body: string): Promise<{ modified: number }>;
  };
  remote: ServerApi;
  now(): number; // epoch 秒
}

/**
 * 执行同步计划（逐条执行，单条失败不中止）
 */
export async function runSync(
  plan: Action[],
  ctx: {
    localByKey: Map<string, LocalNote>;
    io: SyncIO;
    state: SyncState;
  },
): Promise<SyncReport> {
  const { localByKey, io, state } = ctx;
  const report: SyncReport = {
    items: [],
    uploaded: 0,
    downloaded: 0,
    conflicts: 0,
    restored: 0,
    deletedRemote: 0,
    skipped: 0,
    errors: 0,
  };

  // 辅助：截断标题至 64 字
  const truncTitle = (t: string): string => {
    const chars = [...t];
    return chars.length > 64 ? chars.slice(0, 64).join("") : t;
  };

  for (const action of plan) {
    try {
      switch (action.kind) {
        case "skip-incompatible":
        case "skip-empty":
        case "skip-conflict-copy": {
          report.items.push({ action, ok: true });
          report.skipped++;
          break;
        }
        case "upload": {
          const note = localByKey.get(action.key);
          if (!note) throw new Error(`本地笔记不存在: ${action.key}`);
          const title = truncTitle(note.title);
          const result = await io.remote.putPage(action.ref, {
            title,
            summary: "",
            content: note.body,
          });
          // locked 检测
          if (result.content !== note.body) {
            report.items.push({ action, ok: false, detail: "locked-skipped" });
            report.errors++;
            break;
          }
          state.entries[action.key] = {
            ref: action.ref,
            localHash: note.contentHash,
            remoteUpdatedAt: result.updated_at,
          };
          report.items.push({ action, ok: true });
          report.uploaded++;
          break;
        }
        case "restore-remote": {
          const note = localByKey.get(action.key);
          if (!note) throw new Error(`本地笔记不存在: ${action.key}`);
          const title = truncTitle(note.title);
          const result = await io.remote.putPage(action.ref, {
            title,
            summary: "",
            content: note.body,
          });
          if (result.content !== note.body) {
            report.items.push({ action, ok: false, detail: "locked-skipped" });
            report.errors++;
            break;
          }
          state.entries[action.key] = {
            ref: action.ref,
            localHash: note.contentHash,
            remoteUpdatedAt: result.updated_at,
          };
          report.items.push({ action, ok: true });
          report.restored++;
          break;
        }
        case "download": {
          const remotePage = await io.remote.getPage(action.ref);
          if (!remotePage) throw new Error(`远端页面不存在: ${action.ref}`);
          await io.local.writeNote(action.key, remotePage.title, remotePage.content);
          const h = await hashNote(remotePage.title, remotePage.content);
          state.entries[action.key] = {
            ref: action.ref,
            localHash: h,
            remoteUpdatedAt: remotePage.updated_at,
          };
          report.items.push({ action, ok: true });
          report.downloaded++;
          break;
        }
        case "delete-remote": {
          await io.remote.deletePage(action.ref);
          delete state.entries[action.key];
          report.items.push({ action, ok: true });
          report.deletedRemote++;
          break;
        }
        case "drop-state": {
          delete state.entries[action.key];
          report.items.push({ action, ok: true });
          report.skipped++;
          break;
        }
        case "conflict-local-wins": {
          // 输方为远端：先保存远端副本
          const note = localByKey.get(action.key);
          if (!note) throw new Error(`本地笔记不存在: ${action.key}`);
          const remotePage = await io.remote.getPage(action.ref);
          if (remotePage) {
            const copyKey = conflictCopyKey(action.key, io.now());
            await io.local.writeNote(copyKey, action.key, remotePage.content);
          }
          // 再执行本地覆盖远端
          const title = truncTitle(note.title);
          const result = await io.remote.putPage(action.ref, {
            title,
            summary: "",
            content: note.body,
          });
          if (result.content !== note.body) {
            report.items.push({ action, ok: false, detail: "locked-skipped" });
            report.errors++;
            break;
          }
          state.entries[action.key] = {
            ref: action.ref,
            localHash: note.contentHash,
            remoteUpdatedAt: result.updated_at,
          };
          report.items.push({ action, ok: true });
          report.conflicts++;
          break;
        }
        case "conflict-remote-wins": {
          // 输方为本地：先保存本地副本
          const note = localByKey.get(action.key);
          if (!note) throw new Error(`本地笔记不存在: ${action.key}`);
          const copyKey = conflictCopyKey(action.key, io.now());
          await io.local.writeNote(copyKey, action.key, note.body);
          // 再下载远端
          const remotePage = await io.remote.getPage(action.ref);
          if (!remotePage) throw new Error(`远端页面不存在: ${action.ref}`);
          await io.local.writeNote(action.key, remotePage.title, remotePage.content);
          const h = await hashNote(remotePage.title, remotePage.content);
          state.entries[action.key] = {
            ref: action.ref,
            localHash: h,
            remoteUpdatedAt: remotePage.updated_at,
          };
          report.items.push({ action, ok: true });
          report.conflicts++;
          break;
        }
        default: {
          const _exhaustive: never = action;
          throw new Error(`未知 action: ${String(_exhaustive)}`);
        }
      }
    } catch (e) {
      const detail = e instanceof Error ? e.message : String(e);
      report.items.push({ action, ok: false, detail });
      report.errors++;
    }
  }

  return report;
}
