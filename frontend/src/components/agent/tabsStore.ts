/**
 * 多会话标签页的纯函数状态模块（浏览器 tab 式）。
 * 持久化于 localStorage：`agent.openTabs.<workspaceId>`（JSON：{ open, active }）。
 * 所有函数保持纯/幂等，便于单测与 React state 直接复用。
 * localStorage 访问统一走 safeStorage 包装（隐私模式/禁用时不抛异常）。
 */

import { safeLocalStorageGet, safeLocalStorageRemove, safeLocalStorageSet } from './safeStorage';

export interface TabState {
  /** 已打开标签的会话 id，有序，front = 最早打开 */
  open: string[];
  /** 当前激活标签的会话 id（'' = 无激活） */
  active: string;
}

export const MAX_TABS = 10;

const storageKey = (workspaceId: string) => `agent.openTabs.${workspaceId}`;

/**
 * 读取工作区的标签状态。损坏/格式非法/不存在 → null。
 * active 不在 open 中则修正为 open[0] ?? ''。
 */
export function loadTabs(workspaceId: string): TabState | null {
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

/** 写入工作区标签状态。localStorage 异常（容量满等）静默丢弃——本地记忆仅是增强。 */
export function saveTabs(workspaceId: string, state: TabState): void {
  safeLocalStorageSet(storageKey(workspaceId), JSON.stringify(state));
}

/**
 * 单标签时代的迁移：若 localStorage 的「最近工作区」与 workspaceId 匹配且
 * 存在「最近会话」，迁移为单标签状态并删除两个旧 key，否则返回 null。
 * 注意调用方仍会继续写 agent.lastWorkspaceId（工作区记忆逻辑不变）。
 */
export function migrateLegacy(workspaceId: string): TabState | null {
  try {
    if (safeLocalStorageGet('agent.lastWorkspaceId') !== workspaceId) return null;
    const last = safeLocalStorageGet('agent.lastSessionId');
    if (!last) return null;
    safeLocalStorageRemove('agent.lastWorkspaceId');
    safeLocalStorageRemove('agent.lastSessionId');
    return { open: [last], active: last };
  } catch {
    return null;
  }
}

/** 对齐会话列表：过滤 open 中已不存在的 id；active 失效取剩余首个，空则 ''。 */
export function reconcile(state: TabState, sessionIds: string[]): TabState {
  const ids = new Set(sessionIds);
  const open = state.open.filter((id) => ids.has(id));
  const active = open.includes(state.active) ? state.active : open[0] ?? '';
  return { open, active };
}

/** 打开或激活：已在 open 中仅激活；否则追加并激活，超 MAX_TABS 时 FIFO 淘汰最早。 */
export function openOrActivate(state: TabState, id: string): TabState {
  if (state.open.includes(id)) return { ...state, active: id };
  const open = [...state.open, id];
  if (open.length > MAX_TABS) open.shift();
  return { open, active: id };
}

/** 关闭标签：移除 id；若关的是 active，激活邻居（优先右侧，否则左侧，否则 ''）。 */
export function closeTab(state: TabState, id: string): TabState {
  if (!state.open.includes(id)) return state;
  const open = state.open.filter((x) => x !== id);
  let active = state.active;
  if (active === id) {
    const idx = state.open.indexOf(id);
    active = open[idx] ?? open[idx - 1] ?? '';
  }
  return { open, active };
}
