import { deriveRefFromKey, toRemoteRef } from "./ref-id";
import { isConflictCopyKey } from "./sync-engine";
import type { LocalNote } from "./sync-engine";

/**
 * 预处理：为不兼容的笔记自动派生确定性 ref 并回写 frontmatter
 * 仅对“refId 为空且 key 不兼容”的笔记派生；其它情况跳过
 * @param notes 本地笔记（会原地更新 refId/modified）
 * @param setRef 回写函数
 * @returns 成功回写数量
 */
export async function ensureCompatibleRefs(
  notes: LocalNote[],
  setRef: (key: string, ref: string) => Promise<{ modified: number }>,
): Promise<number> {
  let count = 0;
  for (const note of notes) {
    if (isConflictCopyKey(note.key)) continue;
    if (note.body.trim() === "") continue;
    const remote = toRemoteRef(note.key, note.refId);
    if (remote != null) continue;
    // refId 非空但非法（toRemoteRef 因 frontmatter 非法返回 null）时跳过——不覆盖用户手写的非法 ref
    if (note.refId != null && note.refId.trim() !== "") continue;
    // 仅对“refId 为空且 key 不兼容”的笔记派生
    const ref = await deriveRefFromKey(note.key);
    try {
      const res = await setRef(note.key, ref);
      note.refId = ref;
      note.modified = res.modified;
      count++;
    } catch (e) {
      console.warn(`ensureCompatibleRefs: setRef failed for ${note.key}`, e);
    }
  }
  return count;
}
