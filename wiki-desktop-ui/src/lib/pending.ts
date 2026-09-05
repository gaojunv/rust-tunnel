import { hashNote, isConflictCopyKey } from "./sync-engine";
import type { SyncState } from "./sync-engine";
import { toRemoteRef } from "./ref-id";

/**
 * 计算待同步笔记数量（纯函数）
 * - 对每篇笔记计算 hashNote(title, body) 并与 state.entries[key]?.localHash 对比
 * - 新 key（entries 中不存在）计为待上传
 * - 跳过冲突副本 / 不兼容 key / 空内容（与 planSync 跳过规则一致）
 */
export async function computePendingCount(
  notes: { key: string; title: string; body: string; refId?: string | null }[],
  state: SyncState | null,
): Promise<number> {
  let count = 0;
  for (const n of notes) {
    if (isConflictCopyKey(n.key)) continue;
    const refId = (n as { refId?: string | null }).refId ?? null;
    const ref = toRemoteRef(n.key, refId);
    if (ref == null) continue;
    if (n.body.trim() === "") continue;
    const h = await hashNote(n.title, n.body);
    const entry = state?.entries[n.key];
    if (!entry || h !== entry.localHash) count++;
  }
  return count;
}
