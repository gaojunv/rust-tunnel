import { hashNotesBatch, isConflictCopyKey } from "./sync-engine";
import type { SyncState } from "./sync-engine";
import { toRemoteRef } from "./ref-id";

/**
 * 计算待同步笔记数量（纯函数）
 * - 对每篇笔记计算 hash 并与 state.entries[key]?.localHash 对比
 * - 新 key（entries 中不存在）计为待上传
 * - 跳过冲突副本 / 不兼容 key / 空内容（与 planSync 跳过规则一致）
 * - hash 经 hashNotesBatch 分批并行计算，与逐个 await 结果等价
 */
export async function computePendingCount(
  notes: { key: string; title: string; body: string; refId?: string | null }[],
  state: SyncState | null,
  precomputed?: Map<string, string>,
): Promise<number> {
  // 单遍判定 eligible（跳过规则与 planSync 一致），hash 槽位与 eligible 一一对应
  const keys: string[] = [];
  const toHash: { title: string; body: string }[] = [];
  const hashes: (string | undefined)[] = [];
  for (const n of notes) {
    if (isConflictCopyKey(n.key)) continue;
    const refId = (n as { refId?: string | null }).refId ?? null;
    if (toRemoteRef(n.key, refId) == null) continue;
    if (n.body.trim() === "") continue;
    keys.push(n.key);
    // precomputed 命中（doSync 刚算好且内容未被改写）时直接用，跳过 hash
    const cached = precomputed?.get(n.key);
    if (cached !== undefined) {
      hashes.push(cached);
    } else {
      toHash.push({ title: n.title, body: n.body });
      hashes.push(undefined);
    }
  }
  if (toHash.length > 0) {
    const computed = await hashNotesBatch(toHash);
    let j = 0;
    for (let i = 0; i < hashes.length; i++) {
      if (hashes[i] === undefined) hashes[i] = computed[j++];
    }
  }
  let count = 0;
  for (let i = 0; i < keys.length; i++) {
    const entry = state?.entries[keys[i]];
    if (!entry || hashes[i] !== entry.localHash) count++;
  }
  return count;
}
