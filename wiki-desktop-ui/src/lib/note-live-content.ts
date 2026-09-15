/**
 * 笔记实时内容广播 —— 轻量 pub/sub（内存同步，无后端）
 * - NoteEditor 在文档变化时 publish({ noteKey, body, cursorLine })
 * - TocPanel 等订阅者 subscribe 回调收到最新快照
 * - 快照按 noteKey 隔离；getSnapshot 供订阅者挂载时取初值
 */

export interface NoteLiveSnapshot {
  noteKey: string;
  body: string;
  /** 光标所在行（0 起），未知时为 null */
  cursorLine: number | null;
}

type Listener = (snapshot: NoteLiveSnapshot) => void;

const listeners = new Set<Listener>();
const latestByKey = new Map<string, NoteLiveSnapshot>();

export function publishNoteLiveContent(snapshot: NoteLiveSnapshot): void {
  latestByKey.set(snapshot.noteKey, snapshot);
  for (const listener of [...listeners]) {
    try {
      listener(snapshot);
    } catch {
      // 单个订阅者异常不影响其他订阅者
    }
  }
}

export function subscribeNoteLiveContent(listener: Listener): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** 取某笔记最近一次发布的快照（未发布过返回 null）。 */
export function getNoteLiveSnapshot(noteKey: string): NoteLiveSnapshot | null {
  return latestByKey.get(noteKey) ?? null;
}

/** 笔记卸载/切换时清理其快照，避免旧内容污染新会话。 */
export function clearNoteLiveSnapshot(noteKey: string): void {
  latestByKey.delete(noteKey);
}

/** 仅供单测：清空全部订阅与快照。 */
export function __resetNoteLiveContentForTest(): void {
  listeners.clear();
  latestByKey.clear();
}
