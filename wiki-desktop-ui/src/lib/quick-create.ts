/**
 * QuickSwitcher 快捷新建的纯判定逻辑（方便单测）。
 *
 * 输入用户 query 与现有 key 集合，输出创建项应呈现的状态：
 *  - hidden：无 query，或调用方未接入 onCreate，不展示创建项
 *  - duplicate：已存在同名笔记，不展示创建项（该笔记本身会在结果中）
 *  - invalid：query 过不了 validateNoteKey，展示禁用项 + 错误提示
 *  - ready：可创建，key 为 normalize 后的值
 */
import { normalizeNoteKey, validateNoteKey } from "./note-key";

export type QuickCreateState =
  | { kind: "hidden" }
  | { kind: "duplicate" }
  | { kind: "invalid"; error: string }
  | { kind: "ready"; key: string };

export function resolveQuickCreate(
  query: string,
  existingKeys: Iterable<string>,
  canCreate: boolean,
): QuickCreateState {
  const trimmed = query.trim();
  if (!canCreate || !trimmed) return { kind: "hidden" };
  const err = validateNoteKey(trimmed);
  if (err) return { kind: "invalid", error: err };
  const key = normalizeNoteKey(trimmed);
  for (const k of existingKeys) {
    if (k === key) return { kind: "duplicate" };
  }
  return { kind: "ready", key };
}
