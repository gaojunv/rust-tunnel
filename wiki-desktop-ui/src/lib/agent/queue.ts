/**
 * 消息排队队列 —— 纯 reducer，无副作用（vitest node 环境可测）
 *
 * 约定（对照设计文档 2B）：
 * - 队列元素只存用户原始文本，当前笔记上下文在实际发送（turnStart）时才注入，
 *   避免 chip 快照过期。
 * - session-only：刷新即丢，不做持久化（调用方不承诺持久化）。
 * - 所有函数返回新数组，不 mutate 入参。
 */

/** 队列元素：只存原始文本，笔记上下文发送时再注入 */
export type QueueItem = {
  /** 入队时生成的唯一 id */
  id: string;
  /** 用户原始输入文本（未注入笔记上下文） */
  text: string;
  /** 入队时间戳（ms） */
  createdAt: number;
};

/** 容量上限：超出拒入（静默拒绝，由调用方提示用户） */
export const MAX_QUEUE_SIZE = 20;

// id 生成计数器（模块级，仅保证同一会话内唯一；可注入性由 createdAt 参数提供）
let seq = 0;

function nextId(createdAt: number): string {
  seq += 1;
  return `q-${createdAt.toString(36)}-${seq.toString(36)}`;
}

/**
 * 入队：空文本拒入，满容量（>= MAX_QUEUE_SIZE）拒入。
 * 拒入时返回原队列内容的拷贝与 item: null。
 */
export function enqueue(
  queue: readonly QueueItem[],
  text: string,
  createdAt: number = Date.now(),
): { queue: QueueItem[]; item: QueueItem | null } {
  const trimmed = text.trim();
  if (!trimmed) return { queue: [...queue], item: null };
  if (queue.length >= MAX_QUEUE_SIZE) return { queue: [...queue], item: null };
  const item: QueueItem = { id: nextId(createdAt), text: trimmed, createdAt };
  return { queue: [...queue, item], item };
}

/**
 * 出队：弹出队首。空队列返回 `{ queue: [], item: null }`，不抛错。
 */
export function dequeue(queue: readonly QueueItem[]): { queue: QueueItem[]; item: QueueItem | null } {
  if (queue.length === 0) return { queue: [], item: null };
  const [head, ...rest] = queue;
  // length > 0 已保证 head 存在
  return { queue: rest, item: head as QueueItem };
}

/** 按 id 删除：id 不存在时返回等内容拷贝，不抛错 */
export function remove(queue: readonly QueueItem[], id: string): QueueItem[] {
  const next = queue.filter((it) => it.id !== id);
  return next.length === queue.length ? [...queue] : next;
}

/** 查看队首：空队列返回 null，不抛错 */
export function peek(queue: readonly QueueItem[]): QueueItem | null {
  if (queue.length === 0) return null;
  return queue[0] as QueueItem;
}

/**
 * 回合完成钩子：弹出队首作为下一条待发送消息。
 * 由 2E 在 `turn/completed` 后调用，取 `toSend` 发起下一次 turnStart。
 */
export function onTurnCompleted(queue: readonly QueueItem[]): {
  next: QueueItem[];
  toSend: QueueItem | null;
} {
  const { queue: next, item } = dequeue(queue);
  return { next, toSend: item };
}

/** 测试辅助：重置 id 计数器 */
export function __resetQueueSeqForTest(): void {
  seq = 0;
}
