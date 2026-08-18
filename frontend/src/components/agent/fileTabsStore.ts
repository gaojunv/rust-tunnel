/**
 * 文件面板多标签 + 未保存草稿的纯函数状态模块。
 * 标签持久化于 localStorage：`agent.files.<workspaceId>`（JSON：{ open, active }）；
 * 草稿只存内存（大文件容量限制，不落盘），key 为 `workspaceId + \0 + path`。
 * 纯函数供单测与 React state 直接复用，localStorage 访问统一走 safeStorage 包装。
 */

import { safeLocalStorageGet, safeLocalStorageSet } from './safeStorage';

export interface FileTabsState {
  /** 已打开文件的路径，有序，front = 最早打开 */
  open: string[];
  /** 当前激活文件的路径（'' = 无激活） */
  active: string;
}

export const MAX_OPEN_FILES = 12;

const storageKey = (workspaceId: string) => `agent.files.${workspaceId}`;

/**
 * 读取工作区已打开文件。损坏/格式非法/不存在 → null。
 * active 不在 open 中则修正为 open[0] ?? ''。
 */
export function loadOpenFiles(workspaceId: string): FileTabsState | null {
  try {
    const raw = safeLocalStorageGet(storageKey(workspaceId));
    if (raw == null) return null;
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== 'object' || parsed === null) return null;
    const obj = parsed as Record<string, unknown>;
    if (!Array.isArray(obj.open)) return null;
    const open = obj.open.filter((x): x is string => typeof x === 'string');
    if (typeof obj.active !== 'string') return null;
    const active = open.includes(obj.active) ? obj.active : open[0] ?? '';
    return { open, active };
  } catch {
    return null;
  }
}

/** 写入工作区打开的文件状态。localStorage 异常（容量满等）静默丢弃——本地记忆仅是增强。 */
export function saveOpenFiles(workspaceId: string, state: FileTabsState): void {
  safeLocalStorageSet(storageKey(workspaceId), JSON.stringify(state));
}

/** 打开或激活：已在 open 中仅激活；否则追加并激活，超 MAX_OPEN_FILES 时 FIFO 淘汰最早。 */
export function openOrActivate(state: FileTabsState, path: string): FileTabsState {
  if (state.open.includes(path)) return { ...state, active: path };
  const open = [...state.open, path];
  if (open.length > MAX_OPEN_FILES) open.shift();
  return { open, active: path };
}

/** 关闭文件：移除 path；若关的是 active，激活邻居（优先右侧，否则左侧，否则 ''）。 */
export function closePath(state: FileTabsState, path: string): FileTabsState {
  if (!state.open.includes(path)) return state;
  const open = state.open.filter((x) => x !== path);
  let active = state.active;
  if (active === path) {
    const idx = state.open.indexOf(path);
    active = open[idx] ?? open[idx - 1] ?? '';
  }
  return { open, active };
}

// ── 未保存草稿（模块内存 store，不落盘）────────────────────────────

interface DraftEntry {
  draft: string;
  dirty: boolean;
}

const drafts = new Map<string, DraftEntry>();

const draftKey = (workspaceId: string, path: string) => `${workspaceId}\0${path}`;

/** 读取指定文件草稿；无草稿 → null。 */
export function readDraft(workspaceId: string, path: string): DraftEntry | null {
  return drafts.get(draftKey(workspaceId, path)) ?? null;
}

/** 写入草稿并标记未保存（dirty）。 */
export function writeDraft(workspaceId: string, path: string, draft: string): void {
  drafts.set(draftKey(workspaceId, path), { draft, dirty: true });
  emitDraftsChanged();
}

/** 清除草稿（保存成功/内容回到已保存状态）。 */
export function clearDraft(workspaceId: string, path: string): void {
  drafts.delete(draftKey(workspaceId, path));
  emitDraftsChanged();
}

/** 该文件是否存在未保存草稿。 */
export function isDirty(workspaceId: string, path: string): boolean {
  return drafts.get(draftKey(workspaceId, path))?.dirty ?? false;
}

// 草稿变更订阅：标签条「●未保存」圆点需要跟随编辑/保存刷新
const draftListeners = new Set<() => void>();

function emitDraftsChanged(): void {
  for (const listener of draftListeners) listener();
}

/** 订阅草稿变更（write/clear），返回退订函数。 */
export function onDraftsChanged(listener: () => void): () => void {
  draftListeners.add(listener);
  return () => {
    draftListeners.delete(listener);
  };
}