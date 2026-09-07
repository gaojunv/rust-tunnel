import { describe, it, expect } from "vitest";
import {
  createInitialView,
  hydrateTurnsToItems,
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

  it("turn/completed 携带 turn.error 时追加 error item（TurnError 结构）", () => {
    let s = createInitialView("t1");
    s = { ...s, activeTurnId: "turn-9", status: "running" };
    s = reduceAgentEvent(
      s,
      notif("item/started", { item: { type: "agentMessage", id: "a1", text: "partial" } }),
    );
    s = reduceAgentEvent(
      s,
      notif("turn/completed", {
        turn: {
          id: "turn-9",
          status: "failed",
          error: {
            message: "上游请求失败",
            codexErrorInfo: "rateLimitExceeded",
            additionalDetails: "请在 60 秒后重试",
            misalignment: null,
          },
        },
      }),
    );
    expect(s.activeTurnId).toBeNull();
    expect(s.status).toBe("idle");
    const err = s.items.find((i) => i.kind === "error");
    expect(err).toMatchObject({
      kind: "error",
      id: "error-turn-9",
      message: "上游请求失败\n请在 60 秒后重试",
    });
    // 流式项正常收尾，但错误被显式展示（非静默）
    expect(s.items.find((i) => i.id === "a1")).toMatchObject({ kind: "agentMessage", complete: true });
  });

  it("turn/completed 顶层 params.error（字符串）同样生成 error item", () => {
    let s = createInitialView("t1");
    s = { ...s, activeTurnId: "turn-9", status: "running" };
    s = reduceAgentEvent(s, notif("turn/completed", { turn: { id: "turn-9" }, error: "连接中断" }));
    const err = s.items.find((i) => i.kind === "error");
    expect(err).toMatchObject({ kind: "error", message: "连接中断" });
  });

  it("同一 turn 重复 error 不重复追加", () => {
    let s = createInitialView("t1");
    s = { ...s, activeTurnId: "turn-9", status: "running" };
    s = reduceAgentEvent(
      s,
      notif("turn/completed", { turn: { id: "turn-9" }, error: "boom" }),
    );
    const before = s.items.filter((i) => i.kind === "error").length;
    // 后续事件若再带同一 turn 错误（理论不发生）不追加
    s = { ...s, activeTurnId: "turn-9", status: "running" };
    s = reduceAgentEvent(
      s,
      notif("turn/completed", { turn: { id: "turn-9" }, error: "boom" }),
    );
    expect(s.items.filter((i) => i.kind === "error").length).toBe(before);
  });

  it("无 error 的 turn/completed 不生成 error item", () => {
    let s = createInitialView("t1");
    s = { ...s, activeTurnId: "turn-9", status: "running" };
    s = reduceAgentEvent(s, notif("turn/completed", { turn: { id: "turn-9", error: null } }));
    expect(s.items.some((i) => i.kind === "error")).toBe(false);
  });

  it("顶层 error 通知（ErrorNotification）生成 error item", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(
      s,
      notif("error", {
        error: { message: "模型限流", additionalDetails: null },
        willRetry: true,
        threadId: "t1",
        turnId: "turn-5",
      }),
    );
    expect(s.items.find((i) => i.kind === "error")).toMatchObject({
      kind: "error",
      id: "error-turn-5",
      message: "模型限流",
    });
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

  it("turn/diff/updated 写入 turnDiffs（与 fileChange 解耦，不再改动 fileChange）", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(
      s,
      notif("item/started", {
        item: { type: "fileChange", id: "f1", changes: [{ path: "x.md", kind: "update", diff: "base" }], status: "inProgress" },
      }),
    );
    s = reduceAgentEvent(s, notif("turn/diff/updated", { turnId: "turn-1", diff: "++added" }));
    expect(s.turnDiffs).toEqual([{ turnId: "turn-1", diff: "++added" }]);
    // fileChange 自身 diff 不被改动（MessageStream 预览来源不变）
    const f = s.items.find((i) => i.id === "f1") as Extract<(typeof s.items)[number], { kind: "fileChange" }>;
    expect(f.files[0].diff).toBe("base");
  });

  it("turn/diff/updated 同 turnId 重复通知替换内容、不同 turnId 各自累积", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(s, notif("turn/diff/updated", { turnId: "turn-1", diff: "v1" }));
    s = reduceAgentEvent(s, notif("turn/diff/updated", { turnId: "turn-1", diff: "v2" }));
    expect(s.turnDiffs).toEqual([{ turnId: "turn-1", diff: "v2" }]);
    s = reduceAgentEvent(s, notif("turn/diff/updated", { turnId: "turn-2", diff: "other" }));
    expect(s.turnDiffs).toEqual([
      { turnId: "turn-1", diff: "v2" },
      { turnId: "turn-2", diff: "other" },
    ]);
  });

  it("turn/diff/updated 内容一致时返回原状态；无 turnId（且无活跃 turn）时忽略", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(s, notif("turn/diff/updated", { turnId: "turn-1", diff: "v1" }));
    const same = reduceAgentEvent(s, notif("turn/diff/updated", { turnId: "turn-1", diff: "v1" }));
    expect(same).toBe(s);
    // 无 turnId 且无 activeTurnId：忽略（不新增）
    const ignored = reduceAgentEvent(s, notif("turn/diff/updated", { diff: "zzz" }));
    expect(ignored).toBe(s);
    expect(ignored.turnDiffs.length).toBe(1);
  });

  it("turn/diff/updated 以 activeTurnId 兜底 turnId；空 diff 忽略", () => {
    let s = createInitialView("t1");
    s = { ...s, activeTurnId: "turn-9", status: "running" };
    s = reduceAgentEvent(s, notif("turn/diff/updated", { diff: "snapshot" }));
    expect(s.turnDiffs).toEqual([{ turnId: "turn-9", diff: "snapshot" }]);
    const before = s;
    s = reduceAgentEvent(s, notif("turn/diff/updated", { turnId: "turn-9", diff: "" }));
    expect(s).toBe(before);
  });

  it("无 fileChange 时 turn/diff/updated 仍记录（旧 hack 会丢失）", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(s, notif("turn/diff/updated", { turnId: "turn-1", diff: "d" }));
    expect(s.turnDiffs.length).toBe(1);
    expect(s.items.length).toBe(0);
  });

  it("thread/started 切换 thread 时清空 turnDiffs（同 thread 保留）", () => {
    let s = createInitialView("t-old");
    s = reduceAgentEvent(s, notif("turn/diff/updated", { turnId: "turn-1", diff: "d" }));
    expect(s.turnDiffs.length).toBe(1);
    const same = reduceAgentEvent(s, notif("thread/started", { thread: { id: "t-old" } }));
    expect(same.turnDiffs.length).toBe(1);
    const next = reduceAgentEvent(s, notif("thread/started", { thread: { id: "t-new" } }));
    expect(next.turnDiffs).toEqual([]);
    expect(next.items).toEqual([]);
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

  it("warning 通知生成 tone=warn system item", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(
      s,
      notif("warning", { threadId: "t1", message: "配置目录不可写，已降级为只读" }),
    );
    const sys = s.items.find((i) => i.kind === "system");
    expect(sys).toMatchObject({
      kind: "system",
      id: "system-warning-t1",
      text: "配置目录不可写，已降级为只读",
      tone: "warn",
    });
  });

  it("warning 重复相同到达只保留一条（upsert 去重）", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(s, notif("warning", { threadId: null, message: "同一提示" }));
    const first = s;
    s = reduceAgentEvent(s, notif("warning", { threadId: null, message: "同一提示" }));
    expect(s.items.filter((i) => i.kind === "system").length).toBe(1);
    expect(s).toBe(first);
    // 内容变化则替换（仍单条）
    s = reduceAgentEvent(s, notif("warning", { threadId: null, message: "新提示" }));
    expect(s.items.filter((i) => i.kind === "system").length).toBe(1);
    expect(s.items.find((i) => i.kind === "system")).toMatchObject({ text: "新提示" });
  });

  it("configWarning 带 path/details 生成 warn system item", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(
      s,
      notif("configWarning", {
        summary: "未知字段",
        details: "将忽略该配置项",
        path: "/vault/.codex/config.toml",
        range: null,
      }),
    );
    const sys = s.items.find((i) => i.kind === "system");
    expect(sys).toMatchObject({
      kind: "system",
      text: "未知字段（/vault/.codex/config.toml）\n将忽略该配置项",
      tone: "warn",
    });
  });

  it("model/rerouted 生成 tone=info system item（含模型切换文案）", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(
      s,
      notif("model/rerouted", {
        threadId: "t1",
        turnId: "turn-9",
        fromModel: "model-a",
        toModel: "model-b",
        reason: "highRiskCyberActivity",
      }),
    );
    const sys = s.items.find((i) => i.kind === "system");
    expect(sys).toMatchObject({
      kind: "system",
      id: "system-model-rerouted-turn-9",
      text: "模型已从 model-a 切换为 model-b：高风险网络活动",
      tone: "info",
    });
  });

  it("thread/compacted 生成 tone=info system item", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(s, notif("thread/compacted", { threadId: "t1", turnId: "turn-3" }));
    const sys = s.items.find((i) => i.kind === "system");
    expect(sys).toMatchObject({
      kind: "system",
      id: "system-thread-compacted-turn-3",
      text: "对话上下文已压缩整理，更早内容保留为摘要",
      tone: "info",
    });
  });

  it("deprecationNotice 生成 tone=warn system item", () => {
    let s = createInitialView("t1");
    s = reduceAgentEvent(
      s,
      notif("deprecationNotice", { summary: "命令已弃用", details: "请改用新语法" }),
    );
    const sys = s.items.find((i) => i.kind === "system");
    expect(sys).toMatchObject({
      kind: "system",
      text: "命令已弃用\n请改用新语法",
      tone: "warn",
    });
  });

  it("warning 无 message/summary 时不生成 system item（model/rerouted 例外）", () => {
    let s = createInitialView("t1");
    const same = reduceAgentEvent(s, notif("warning", { threadId: null, message: "" }));
    expect(same).toBe(s);
    expect(same.items.some((i) => i.kind === "system")).toBe(false);
  });

  it("account/rateLimits/updated 原样写入 rateLimits", () => {
    let s = createInitialView("t1");
    const rl = { limitId: "l1", limitName: "gpt-5", primary: { kind: "rpm" }, secondary: null };
    s = reduceAgentEvent(s, notif("account/rateLimits/updated", { rateLimits: rl }));
    expect(s.rateLimits).toEqual(rl);
    // null 载荷不改变
    s = { ...s, rateLimits: rl };
    const same = reduceAgentEvent(s, notif("account/rateLimits/updated", { rateLimits: null }));
    expect(same).toBe(s);
  });
});

describe("hydrateTurnsToItems（历史回填）", () => {
  it("按 turn 顺序展开并映射可还原类型", () => {
    const turns = [
      {
        id: "turn-1",
        items: [
          { type: "userMessage", id: "u1", content: [{ type: "text", text: "hi" }], clientId: null },
          { type: "agentMessage", id: "a1", text: "hello", phase: null, memoryCitation: null, delivery: null, questions: null },
        ],
      },
      {
        id: "turn-2",
        items: [
          {
            type: "fileChange",
            id: "f1",
            changes: [{ path: "x.md", kind: "update", diff: "d" }],
            status: "completed",
          },
          {
            type: "commandExecution",
            id: "c1",
            command: "echo hi",
            cwd: "/vault",
            aggregatedOutput: "hi\n",
            status: "completed",
          },
          { type: "reasoning", id: "r1", summary: ["think"], content: [] },
        ],
      },
    ];
    const views = hydrateTurnsToItems(turns);
    expect(views.map((v) => v.kind)).toEqual([
      "userMessage",
      "agentMessage",
      "fileChange",
      "commandExecution",
      "reasoning",
    ]);
    expect(views[0]).toMatchObject({ kind: "userMessage", id: "u1", text: "hi" });
    expect(views[1]).toMatchObject({ kind: "agentMessage", id: "a1", text: "hello" });
    expect(views[3]).toMatchObject({ kind: "commandExecution", id: "c1", command: "echo hi" });
  });

  it("跳过 hookPrompt 等无法还原类型与畸形条目", () => {
    const turns = [
      null,
      "junk",
      42,
      {
        id: "turn-1",
        items: [
          { type: "hookPrompt", id: "h1", fragments: [] },
          { type: "mcpToolCall", id: "m1" },
          null,
          "bad",
          { type: "userMessage", id: "u2", content: [{ type: "text", text: "ok" }], clientId: null },
          { type: "mysteryNewType", id: "x1" },
        ],
      },
      { id: "turn-2", items: [] },
      { id: "turn-3" },
    ] as unknown as Parameters<typeof hydrateTurnsToItems>[0];
    const views = hydrateTurnsToItems(turns);
    // 只保留能还原的 userMessage
    expect(views).toHaveLength(1);
    expect(views[0]).toMatchObject({ kind: "userMessage", id: "u2", text: "ok" });
  });

  it("空数组与非数组返回空", () => {
    expect(hydrateTurnsToItems([])).toEqual([]);
    expect(hydrateTurnsToItems(null as unknown as Parameters<typeof hydrateTurnsToItems>[0])).toEqual([]);
    expect(hydrateTurnsToItems(undefined as unknown as Parameters<typeof hydrateTurnsToItems>[0])).toEqual([]);
  });

  it("返回的列表为 ItemView[]（含 complete 语义字段）", () => {
    const turns = [
      {
        id: "turn-1",
        items: [{ type: "agentMessage", id: "a1", text: "done", phase: null, memoryCitation: null, delivery: null, questions: null }],
      },
    ] as unknown as Parameters<typeof hydrateTurnsToItems>[0];
    const views = hydrateTurnsToItems(turns);
    expect(views[0]).toMatchObject({ kind: "agentMessage", complete: false });
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
