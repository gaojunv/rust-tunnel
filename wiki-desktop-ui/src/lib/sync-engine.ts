/**
 * 双向同步核心 —— 纯逻辑 + 依赖注入
 * 方向判定严格按任务说明伪码实现
 */

import { toRemoteRef, sanitizeOriginKey } from "./ref-id";
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
 * 解析远端时间字符串为 epoch 秒
 * 兼容两种格式："YYYY-MM-DD HH:MM:SS"（空格分隔，视为 UTC）与 ISO 串
 * （"2024-01-02T03:04:05Z" / 带小数秒 / 带 +08:00 这类时区偏移）
 * trim 后若已含 T 或已带时区后缀（Z / ±hh:mm / ±hhmm）直接 Date.parse；
 * 否则把空格换成 T 再追加 Z。非法/空返回 0
 */
export function parseRemoteTime(s: string): number {
  try {
    if (!s || typeof s !== "string") return 0;
    const t = s.trim();
    if (t === "") return 0;
    // 已是 ISO 形态（含 T 或已带时区后缀）→ 直接解析，避免重复追加 Z 导致 NaN
    const hasT = t.includes("T");
    const hasTz = /([Zz]|[+-]\d{2}:?\d{2})$/.test(t);
    const iso = hasT || hasTz ? t : t.replace(" ", "T") + "Z";
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
  | { kind: "download-new"; key: string; ref: string }
  | { kind: "conflict-local-wins"; key: string; ref: string }
  | { kind: "conflict-remote-wins"; key: string; ref: string }
  | { kind: "conflict-pending"; key: string; ref: string; localModified: number; remoteUpdatedAt: string }
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
  deferConflicts?: boolean;
}): Action[] {
  const { local, remote, state, propagateDeletes, deferConflicts = false } = input;
  const remoteByRef = new Map<string, RemotePageSummary>();
  for (const r of remote) remoteByRef.set(r.ref, r);

  const seenLocal = new Set<string>();
  const claimedRefs = new Set<string>();
  const localKeys = new Set<string>();
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
    claimedRefs.add(ref);
    localKeys.add(note.key);

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
      } else if (deferConflicts) {
        actions.push({
          kind: "conflict-pending",
          key: note.key,
          ref,
          localModified: note.modified,
          remoteUpdatedAt: r.updated_at,
        });
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
        } else if (deferConflicts) {
          actions.push({
            kind: "conflict-pending",
            key: note.key,
            ref,
            localModified: note.modified,
            remoteUpdatedAt: r.updated_at,
          });
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

  // 服务端独有页面 → 下载（本地新增覆盖不到的远端 ref）
  for (const r of remote) {
    // 已被本地笔记覆盖（主循环中 toRemoteRef 算出该 ref）
    if (claimedRefs.has(r.ref)) continue;
    // 已有同步条目 → 由"本地已删"分支（delete-remote/drop-state）处理
    let tracked = false;
    for (const key of Object.keys(state.entries)) {
      if (state.entries[key].ref === r.ref) {
        tracked = true;
        break;
      }
    }
    if (tracked) continue;
    // 删除墓碑：本地删过且远端未变 → 不复活
    if (state.skipped[r.ref] === r.updated_at) continue;
    // key 选择：origin_key（安全时）优先还原原始路径，被本地占用则退到 ref，
    // 两者都被占才跳过（本地笔记撞上的是别的 ref 的下载目标）
    const preferred = r.origin_key ? sanitizeOriginKey(r.origin_key) : null;
    const candidates = preferred != null && preferred !== r.ref ? [preferred, r.ref] : [r.ref];
    const downloadKey = candidates.find((k) => !localKeys.has(k));
    if (downloadKey == null) {
      actions.push({
        kind: "skip-incompatible",
        key: preferred ?? r.ref,
        reason: `远端页面 "${r.ref}"（origin_key: ${r.origin_key ?? "无"}）的可用本地 key 均被指向别的 ref 的笔记占用，跳过下载`,
      });
      continue;
    }
    actions.push({ kind: "download-new", key: downloadKey, ref: r.ref });
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
    /** 将 sticky ref 写进本地 frontmatter（下载新页面后），可选 */
    setNoteRef?(key: string, ref: string): Promise<unknown>;
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
          const uploadBody: { title: string; summary: string; content: string; origin_key?: string } = {
            title,
            summary: "",
            content: note.body,
          };
          if (note.key !== action.ref) uploadBody.origin_key = note.key;
          const result = await io.remote.putPage(action.ref, uploadBody);
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
          const restoreBody: { title: string; summary: string; content: string; origin_key?: string } = {
            title,
            summary: "",
            content: note.body,
          };
          if (note.key !== action.ref) restoreBody.origin_key = note.key;
          const result = await io.remote.putPage(action.ref, restoreBody);
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
        case "download-new": {
          const remotePage = await io.remote.getPage(action.ref);
          if (!remotePage) throw new Error(`远端页面不存在: ${action.ref}`);
          await io.local.writeNote(action.key, remotePage.title, remotePage.content);
          // key≠ref 时写入 sticky ref，使后续同步收敛到 frontmatter ref
          if (action.key !== action.ref && io.local.setNoteRef) {
            try { await io.local.setNoteRef(action.key, action.ref); } catch (e) { console.warn("setNoteRef failed", e); }
          }
          const h = await hashNote(remotePage.title, remotePage.content);
          state.entries[action.key] = {
            ref: action.ref,
            localHash: h,
            remoteUpdatedAt: remotePage.updated_at,
          };
          delete state.skipped[action.ref];
          report.items.push({ action, ok: true });
          report.downloaded++;
          break;
        }
        case "drop-state": {
          // 删除墓碑：记住远端 updated_at，防止下个周期当作 remote-only 重新下载回来
          const entry = state.entries[action.key];
          if (entry) state.skipped[entry.ref] = entry.remoteUpdatedAt;
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
          const conflictBody: { title: string; summary: string; content: string; origin_key?: string } = {
            title,
            summary: "",
            content: note.body,
          };
          if (note.key !== action.ref) conflictBody.origin_key = note.key;
          const result = await io.remote.putPage(action.ref, conflictBody);
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
        case "conflict-pending": {
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
