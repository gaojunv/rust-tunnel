/**
 * @vitest-environment node
 */
import { describe, it, expect, beforeEach } from "vitest";
import {
  enqueue,
  dequeue,
  remove,
  peek,
  onTurnCompleted,
  MAX_QUEUE_SIZE,
  __resetQueueSeqForTest,
} from "./queue";
import type { QueueItem } from "./queue";

describe("enqueue", () => {
  beforeEach(() => {
    __resetQueueSeqForTest();
  });

  it("尾部追加，id 唯一且递增", () => {
    let q: QueueItem[] = [];
    const a = enqueue(q, "第一条", 1000);
    expect(a.item).not.toBeNull();
    expect(a.queue.length).toBe(1);
    q = a.queue;
    const b = enqueue(q, "第二条", 1001);
    expect(b.queue.length).toBe(2);
    expect(b.queue[0]?.text).toBe("第一条");
    expect(b.queue[1]?.text).toBe("第二条");
    expect(b.queue[0]?.id).not.toBe(b.queue[1]?.id);
  });

  it("存储原始文本（trim 后空才拒），createdAt 透传", () => {
    const { queue } = enqueue([], "  你好，世界  ", 12345);
    expect(queue[0]?.text).toBe("你好，世界");
    expect(queue[0]?.createdAt).toBe(12345);
    expect(queue[0]?.id).toMatch(/^q-/);
  });

  it("空文本 / 纯空白拒入", () => {
    const r1 = enqueue([], "", 0);
    expect(r1.item).toBeNull();
    expect(r1.queue.length).toBe(0);
    const r2 = enqueue([], "   \n\t ", 0);
    expect(r2.item).toBeNull();
  });

  it("满容量拒入：不新增、队列不变", () => {
    let q: QueueItem[] = [];
    for (let i = 0; i < MAX_QUEUE_SIZE; i++) {
      const r = enqueue(q, `m${i}`, 1000 + i);
      q = r.queue;
    }
    expect(q.length).toBe(MAX_QUEUE_SIZE);
    const over = enqueue(q, "溢出", 9999);
    expect(over.item).toBeNull();
    expect(over.queue.length).toBe(MAX_QUEUE_SIZE);
    // 队尾仍是原最后一条
    expect(over.queue[MAX_QUEUE_SIZE - 1]?.text).toBe(`m${MAX_QUEUE_SIZE - 1}`);
  });

  it("满容量边界：恰满时最后一条可入，再一条拒入", () => {
    let q: QueueItem[] = [];
    for (let i = 0; i < MAX_QUEUE_SIZE - 1; i++) {
      q = enqueue(q, `m${i}`, i).queue;
    }
    const last = enqueue(q, "恰满", 500);
    expect(last.item).not.toBeNull();
    expect(last.queue.length).toBe(MAX_QUEUE_SIZE);
    const over = enqueue(last.queue, "再多", 501);
    expect(over.item).toBeNull();
  });
});

describe("dequeue / peek", () => {
  it("dequeue 弹出队首（FIFO）", () => {
    let q = enqueue([], "a", 1).queue;
    q = enqueue(q, "b", 2).queue;
    const { queue, item } = dequeue(q);
    expect(item?.text).toBe("a");
    expect(queue.length).toBe(1);
    expect(queue[0]?.text).toBe("b");
  });

  it("空队列 dequeue / peek 返回 null 不抛错", () => {
    expect(dequeue([]).item).toBeNull();
    expect(dequeue([]).queue).toEqual([]);
    expect(peek([])).toBeNull();
  });

  it("peek 不改队列", () => {
    let q = enqueue([], "a", 1).queue;
    expect(peek(q)?.text).toBe("a");
    expect(q.length).toBe(1);
  });
});

describe("remove", () => {
  it("按 id 删除", () => {
    let q = enqueue([], "a", 1).queue;
    q = enqueue(q, "b", 2).queue;
    const idA = q[0]?.id as string;
    const idB = q[1]?.id as string;
    const next = remove(q, idA);
    expect(next.length).toBe(1);
    expect(next[0]?.id).toBe(idB);
  });

  it("id 不存在时返回等长拷贝，不抛错、不影响原队列", () => {
    let q = enqueue([], "a", 1).queue;
    const before = q;
    const next = remove(q, "nope");
    expect(next.length).toBe(1);
    expect(q).toBe(before);
  });

  it("空队列 remove 返回空数组", () => {
    expect(remove([], "x")).toEqual([]);
  });
});

describe("onTurnCompleted", () => {
  it("空队列返回 next=[] 与 toSend=null，不抛错", () => {
    const r = onTurnCompleted([]);
    expect(r.next).toEqual([]);
    expect(r.toSend).toBeNull();
  });

  it("弹出队首作为 toSend，剩余入 next", () => {
    let q = enqueue([], "a", 1).queue;
    q = enqueue(q, "b", 2).queue;
    const { next, toSend } = onTurnCompleted(q);
    expect(toSend?.text).toBe("a");
    expect(next.length).toBe(1);
    expect(next[0]?.text).toBe("b");
    // 原始队列不被修改
    expect(q.length).toBe(2);
  });

  it("连续 onTurnCompleted 逐条发送直到空", () => {
    let q = enqueue([], "1", 1).queue;
    q = enqueue(q, "2", 2).queue;
    q = enqueue(q, "3", 3).queue;
    const first = onTurnCompleted(q);
    expect(first.toSend?.text).toBe("1");
    const second = onTurnCompleted(first.next);
    expect(second.toSend?.text).toBe("2");
    const third = onTurnCompleted(second.next);
    expect(third.toSend?.text).toBe("3");
    expect(third.next.length).toBe(0);
    const empty = onTurnCompleted(third.next);
    expect(empty.toSend).toBeNull();
  });
});
