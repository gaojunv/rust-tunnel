import { conflictCopyKey, hashNote } from "./sync-engine";
import type { SyncState, SyncReport } from "./sync-engine";

export type PendingConflict = {
  key: string;
  ref: string;
  localModified: number;
  remoteUpdatedAt: string;
};

export type Resolution =
  | "local"
  | "remote"
  | "both"
  | { merged: { title: string; body: string } };

export type ConflictIO = {
  writeNote(key: string, body: string, title?: string): Promise<unknown>;
  putPage(
    ref: string,
    page: { title: string; summary: string; content: string },
  ): Promise<{ updated_at: string; content: string }>;
  now(): number;
};

export function conflictsFromReport(report: SyncReport): PendingConflict[] {
  return report.items
    .filter((it) => it.action.kind === "conflict-pending")
    .map((it) => {
      const a = it.action as { kind: "conflict-pending"; key: string; ref: string; localModified: number; remoteUpdatedAt: string };
      return { key: a.key, ref: a.ref, localModified: a.localModified, remoteUpdatedAt: a.remoteUpdatedAt };
    });
}

function truncTitle(t: string): string {
  const chars = [...t];
  return chars.length > 64 ? chars.slice(0, 64).join("") : t;
}

export async function applyResolution(
  io: ConflictIO,
  state: SyncState,
  c: PendingConflict,
  local: { title: string; body: string },
  remote: { title: string; content: string; updated_at: string },
  res: Resolution,
): Promise<void> {
  const key = c.key;
  const ref = c.ref;

  if (res === "local") {
    const title = truncTitle(local.title);
    const result = await io.putPage(ref, { title, summary: "", content: local.body });
    if (result.content !== local.body) throw new Error("远端页面已锁定");
    state.entries[key] = {
      ref,
      localHash: await hashNote(local.title, local.body),
      remoteUpdatedAt: result.updated_at,
    };
    return;
  }

  if (res === "remote") {
    await io.writeNote(key, remote.content, remote.title);
    state.entries[key] = {
      ref,
      localHash: await hashNote(remote.title, remote.content),
      remoteUpdatedAt: remote.updated_at,
    };
    return;
  }

  if (res === "both") {
    await io.writeNote(conflictCopyKey(key, io.now()), remote.content, remote.title);
    const title = truncTitle(local.title);
    const result = await io.putPage(ref, { title, summary: "", content: local.body });
    if (result.content !== local.body) throw new Error("远端页面已锁定");
    state.entries[key] = {
      ref,
      localHash: await hashNote(local.title, local.body),
      remoteUpdatedAt: result.updated_at,
    };
    return;
  }

  // merged
  const merged = (res as { merged: { title: string; body: string } }).merged;
  await io.writeNote(key, merged.body, merged.title);
  const title = truncTitle(merged.title);
  const result = await io.putPage(ref, { title, summary: "", content: merged.body });
  if (result.content !== merged.body) throw new Error("远端页面已锁定");
  state.entries[key] = {
    ref,
    localHash: await hashNote(merged.title, merged.body),
    remoteUpdatedAt: result.updated_at,
  };
}
