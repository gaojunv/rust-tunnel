import { describe, it, expect } from "vitest";
import {
  createInitialView,
  reduceAgentEvent,
  reduceServerRequest,
  resolveServerRequest,
} from "./codec";
import type { ServerNotification } from "./types";

function notif(method: string, params: unknown): ServerNotification {
  return { method, params } as unknown as ServerNotification;
}

describe("reduceAgentEvent", () => {
  it("item/agentMessage/delta 累积文本到对应 item", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(
      s,
      notif("item/started", {
        item: { type: "agentMessage", id: "a1", text: "" },
      }),
    );
    s = reduceAgentEvent(s, notif("item/agentMessage/delta", { itemId: "a1", delta: "hello " }));
    s = reduceAgentEvent(s, notif("item/agentMessage/delta", { itemId: "a1", delta: "world" }));
    const item = s.items.find((i) => i.id === "a1");
    expect(item).toMatchObject({ kind: "agentMessage", text: "hello world" });
  });

  it("item/reasoning/summaryTextDelta 累积", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(
      s,
      notif("item/started", {
        item: { type: "reasoning", id: "r1", summary: [], content: [] },
      }),
    );
    s = reduceAgentEvent(s, notif("item/reasoning/summaryTextDelta", { itemId: "r1", delta: "think " }));
    s = reduceAgentEvent(s, notif("item/reasoning/textDelta", { itemId: "r1", delta: "more" }));
    const item = s.items.find((i) => i.id === "r1");
    expect(item).toMatchObject({ kind: "reasoning", text: "think more" });
  });

  it("item/commandExecution/outputDelta 累积", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(
      s,
      notif("item/started", {
        item: {
          type: "commandExecution",
          id: "c1",
          command: "echo hi",
          cwd: "/tmp",
          status: "inProgress",
          aggregatedOutput: "",
        },
      }),
    );
    s = reduceAgentEvent(s, notif("item/commandExecution/outputDelta", { itemId: "c1", delta: "line1\n" }));
    s = reduceAgentEvent(s, notif("item/commandExecution/outputDelta", { itemId: "c1", delta: "line2\n" }));
    const item = s.items.find((i) => i.id === "c1");
    expect(item).toMatchObject({ kind: "commandExecution", output: "line1\nline2\n" });
  });

  it("item/completed 定型并标记 complete", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(
      s,
      notif("item/started", {
        item: { type: "agentMessage", id: "a1", text: "partial" },
      }),
    );
    // 通过 completed 覆盖
    s = reduceAgentEvent(
      s,
      notif("item/completed", {
        item: { type: "agentMessage", id: "a1", text: "final text", phase: null },
      }),
    );
    const item = s.items.find((i) => i.id === "a1");
    expect(item).toMatchObject({ kind: "agentMessage", text: "final text", complete: true });
    // 不应重复
    expect(s.items.length).toBe(1);
  });

  it("turn/completed 收尾清空 activeTurnId 并标记未完成的项", () => {
    let s = createInitialView("t1");
    s = { ...s, activeTurnId: "turn-1", status: "running" };
    s = reduceAgentEvent(
      s,
      notif("item/started", {
        item: { type: "agentMessage", id: "a1", text: "hi" },
      }),
    );
    s = reduceAgentEvent(s, notif("turn/completed", { turn: { id: "turn-1" } }));
    expect(s.activeTurnId).toBeNull();
    expect(s.status).toBe("idle");
    const item = s.items.find((i) => i.id === "a1");
    expect(item).toMatchObject({ kind: "agentMessage", complete: true });
  });

  it("thread/tokenUsage/updated 更新 tokenUsage", () => {
    let s = createInitialView("t1");
    const usage = { total: { input: 100, output: 50 }, last: { input: 10, output: 5 } };
    s = reduceAgentEvent(s, notif("thread/tokenUsage/updated", { tokenUsage: usage }));
    expect(s.tokenUsage).toEqual(usage);
  });

  it("未知 method 不抛错且原样返回", () => {
    const s = createInitialView("t1");
    const next = reduceAgentEvent(s, notif("unknown/method", { foo: "bar" }));
    expect(next).toBe(s);
  });

  it("未知 item 类型兜底为 unknown", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(
      s,
      notif("item/completed", {
        item: { type: "mysteryType", id: "m1", foo: "bar" },
      }),
    );
    const item = s.items.find((i) => i.id === "m1");
    expect(item?.kind).toBe("unknown");
  });

  it("thread/started 切换 thread 时重置 items", () => {
    let s = createInitialView("t-old");
    s = {
      ...s,
      items: [{ kind: "userMessage", id: "u1", text: "old" }],
    };
    s = reduceAgentEvent(s, notif("thread/started", { thread: { id: "t-new" } }));
    expect(s.threadId).toBe("t-new");
    expect(s.items.length).toBe(0);
  });

  it("非活跃 turn 的 turn/completed 被忽略", () => {
    let s = createInitialView("t1");
    s = { ...s, activeTurnId: "turn-1", status: "running" };
    s = reduceAgentEvent(s, notif("turn/completed", { turn: { id: "turn-2" } }));
    expect(s.activeTurnId).toBe("turn-1");
  });

  it("fileChange 全生命周期：started/delta/completed", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(
      s,
      notif("item/started", {
        item: {
          type: "fileChange",
          id: "f1",
          changes: [{ path: "a.md", kind: "update", diff: "old" }],
          status: "inProgress",
        },
      }),
    );
    expect(s.items.find((i) => i.id === "f1")).toMatchObject({ kind: "fileChange", files: [{ path: "a.md" }] });
    s = reduceAgentEvent(s, notif("item/fileChange/outputDelta", { itemId: "f1", delta: "+new" }));
    expect((s.items.find((i) => i.id === "f1") as Extract<(typeof s.items)[number], { kind: "fileChange" }>).files[0].diff).toBe("old+new");
    s = reduceAgentEvent(
      s,
      notif("item/completed", {
        item: {
          type: "fileChange",
          id: "f1",
          changes: [{ path: "a.md", kind: "update", diff: "final" }],
          status: "completed",
        },
      }),
    );
    expect(s.items.find((i) => i.id === "f1")).toMatchObject({ kind: "fileChange", status: "completed" });
  });

  it("turn/diff/updated 合并到最近 fileChange", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(
      s,
      notif("item/started", {
        item: { type: "fileChange", id: "f1", changes: [{ path: "x.md", kind: "update", diff: "base" }], status: "inProgress" },
      }),
    );
    s = reduceAgentEvent(s, notif("turn/diff/updated", { diff: "++added" }));
    const f = s.items.find((i) => i.id === "f1") as Extract<(typeof s.items)[number], { kind: "fileChange" }>;
    expect(f.files[0].diff).toContain("++added");
    // 重复相同 diff 不追加
    const before = f.files[0].diff;
    s = reduceAgentEvent(s, notif("turn/diff/updated", { diff: "++added" }));
    expect((s.items.find((i) => i.id === "f1") as Extract<(typeof s.items)[number], { kind: "fileChange" }>).files[0].diff).toBe(before);
  });

  it("turn/plan/updated 聚合为 checklist", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(
      s,
      notif("turn/plan/updated", {
        plan: [
          { title: "step 1", status: "inProgress" },
          { title: "step 2", status: "completed" },
        ],
        explanation: "overall",
      }),
    );
    const plan = s.items.find((i) => i.kind === "plan");
    expect(plan).toBeDefined();
    expect((plan as Extract<typeof plan, { kind: "plan" }>).text).toContain("step 1");
    expect((plan as Extract<typeof plan, { kind: "plan" }>).text).toContain("[x] step 2");
    // 再次更新应替换而非新增
    s = reduceAgentEvent(
      s,
      notif("turn/plan/updated", {
        plan: [{ title: "step 1", status: "completed" }],
        explanation: null,
      }),
    );
    expect(s.items.filter((i) => i.kind === "plan").length).toBe(1);
  });

  it("item/plan/delta 累积", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(s, notif("item/started", { item: { type: "plan", id: "p1", text: "a" } }));
    s = reduceAgentEvent(s, notif("item/plan/delta", { itemId: "p1", delta: " b" }));
    expect(s.items.find((i) => i.id === "p1")).toMatchObject({ kind: "plan", text: "a b" });
  });

  it("fs/changed 与未知事件不抛错", () => {
    let s = createInitialView("t1");
    const next = reduceAgentEvent(s, notif("fs/changed", { watchId: "w1", changedPaths: ["/a.md"] }));
    expect(next).toBe(s);
  });
});

describe("审批队列 reducer", () => {
  it("入队与去重", () => {
    let q = reduceServerRequest([], { id: 1, method: "item/commandExecution/requestApproval", params: {} });
    expect(q.length).toBe(1);
    // 同 id+method 去重
    q = reduceServerRequest(q, { id: 1, method: "item/commandExecution/requestApproval", params: {} });
    expect(q.length).toBe(1);
    q = reduceServerRequest(q, { id: 2, method: "item/fileChange/requestApproval", params: {} });
    expect(q.length).toBe(2);
  });

  it("出队", () => {
    const q1: Array<{ id: unknown; method: string; params: unknown }> = [
      { id: 1 as unknown, method: "a", params: {} },
      { id: 2 as unknown, method: "b", params: {} },
    ];
    const q2 = resolveServerRequest(q1, 1 as unknown);
    expect(q2).toEqual([{ id: 2 as unknown, method: "b", params: {} }]);
  });

  it("字符串与数字 id 等价匹配", () => {
    const q1: Array<{ id: unknown; method: string; params: unknown }> = [
      { id: "42" as unknown, method: "a", params: {} },
    ];
    const q2 = resolveServerRequest(q1, 42 as unknown);
    expect(q2.length).toBe(0);
  });
});
