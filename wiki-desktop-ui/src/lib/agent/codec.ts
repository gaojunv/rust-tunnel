/**
 * 事件 → 视图模型纯函数 reducer（单测核心）
 * 纯 TS，无 DOM、无 Tauri 依赖
 */
import type { ServerNotification } from "./types";

// —— 视图模型 ——

export type ItemView =
  | { kind: "userMessage"; id: string; text: string }
  | { kind: "agentMessage"; id: string; text: string; complete: boolean }
  | { kind: "reasoning"; id: string; text: string; complete?: boolean }
  | {
      kind: "commandExecution";
      id: string;
      command: string;
      output: string;
      exitCode?: number | null;
      status: string;
    }
  | {
      kind: "fileChange";
      id: string;
      files: Array<{ path: string; kind: string; diff: string }>;
      status: string;
    }
  | { kind: "plan"; id: string; text: string }
  | { kind: "unknown"; id: string; raw: unknown };

export type AgentThreadView = {
  threadId: string;
  items: ItemView[];
  activeTurnId?: string | null;
  status: string;
  tokenUsage?: unknown;
};

export type TokenUsageView = unknown;

export function createInitialView(threadId = ""): AgentThreadView {
  return {
    threadId,
    items: [],
    activeTurnId: null,
    status: "idle",
    tokenUsage: null,
  };
}

// —— 审批队列 ——

export type ApprovalRequestView = {
  id: unknown;
  method: string;
  params: unknown;
};

export function reduceServerRequest(
  queue: ApprovalRequestView[],
  req: { id: unknown; method: string; params: unknown },
): ApprovalRequestView[] {
  // 去重：同 id 视为同一请求
  const exists = queue.some((q) => String(q.id) === String(req.id) && q.method === req.method);
  if (exists) return queue;
  return [...queue, { id: req.id, method: req.method, params: req.params }];
}

export function resolveServerRequest(
  queue: ApprovalRequestView[],
  id: unknown,
): ApprovalRequestView[] {
  const sid = String(id);
  return queue.filter((q) => String(q.id) !== sid);
}

// —— 内部：ThreadItem → ItemView ——

function threadItemToView(raw: unknown): ItemView {
  const item = raw as Record<string, unknown>;
  const type = item["type"] as string | undefined;
  const id = (item["id"] as string | undefined) ?? `unknown-${Date.now()}`;
  try {
    switch (type) {
      case "userMessage": {
        const content = item["content"] as Array<Record<string, unknown>> | undefined;
        let text = "";
        if (Array.isArray(content)) {
          text = content
            .map((c) => {
              if (c["type"] === "text" && typeof c["text"] === "string") return c["text"] as string;
              // fallback：序列化
              return "";
            })
            .join("");
          if (!text && content.length > 0) {
            // 兜底：若未提取到文本，尝试取第一个元素的 text
            const first = content[0];
            if (first && typeof first["text"] === "string") text = first["text"] as string;
          }
        } else if (typeof item["text"] === "string") {
          text = item["text"] as string;
        }
        return { kind: "userMessage", id, text };
      }
      case "agentMessage": {
        const text = typeof item["text"] === "string" ? (item["text"] as string) : "";
        return { kind: "agentMessage", id, text, complete: false };
      }
      case "reasoning": {
        // summary + content
        const summary = item["summary"] as string[] | undefined;
        const content = item["content"] as string[] | undefined;
        let text = "";
        if (Array.isArray(summary)) text += summary.join("\n");
        if (Array.isArray(content)) {
          if (text) text += "\n";
          text += content.join("\n");
        }
        if (!text && typeof item["text"] === "string") text = item["text"] as string;
        return { kind: "reasoning", id, text };
      }
      case "commandExecution": {
        const command = typeof item["command"] === "string" ? (item["command"] as string) : "";
        const output =
          typeof item["aggregatedOutput"] === "string"
            ? (item["aggregatedOutput"] as string)
            : typeof item["output"] === "string"
              ? (item["output"] as string)
              : "";
        const status = typeof item["status"] === "string" ? (item["status"] as string) : "inProgress";
        const exitCode = (item["exitCode"] as number | null | undefined) ?? null;
        return { kind: "commandExecution", id, command, output, exitCode, status };
      }
      case "fileChange": {
        const changes = item["changes"] as Array<Record<string, unknown>> | undefined;
        const files = Array.isArray(changes)
          ? changes.map((c) => ({
              path: String(c["path"] ?? ""),
              kind: String(c["kind"] ?? ""),
              diff: String(c["diff"] ?? ""),
            }))
          : [];
        const status = typeof item["status"] === "string" ? (item["status"] as string) : "inProgress";
        return { kind: "fileChange", id, files, status };
      }
      case "plan": {
        const text = typeof item["text"] === "string" ? (item["text"] as string) : "";
        return { kind: "plan", id, text };
      }
      default: {
        return { kind: "unknown", id, raw };
      }
    }
  } catch {
    return { kind: "unknown", id, raw };
  }
}

function upsertItem(items: ItemView[], view: ItemView): ItemView[] {
  const idx = items.findIndex((it) => it.id === view.id);
  if (idx === -1) return [...items, view];
  const next = [...items];
  next[idx] = view;
  return next;
}

function updateItemById(
  items: ItemView[],
  id: string,
  updater: (prev: ItemView) => ItemView,
): ItemView[] {
  const idx = items.findIndex((it) => it.id === id);
  if (idx === -1) return items;
  const next = [...items];
  next[idx] = updater(next[idx]);
  return next;
}

// —— 主 reducer ——

export function reduceAgentEvent(
  state: AgentThreadView,
  event: ServerNotification,
): AgentThreadView {
  const anyEvent = event as unknown as { method: string; params: unknown };
  const method = anyEvent.method;
  const params = anyEvent.params as Record<string, unknown> | null | undefined;

  try {
    switch (method) {
      case "thread/started": {
        const thread = (params as Record<string, unknown>)?.["thread"] as
          | Record<string, unknown>
          | undefined;
        const threadId = (thread?.["id"] as string | undefined) ?? (params as Record<string, unknown>)?.["threadId"] as string | undefined;
        if (!threadId) return state;
        // 若切换 thread，重置 items；同 thread 则保留
        if (state.threadId && state.threadId === threadId) {
          return { ...state, status: "idle" };
        }
        return {
          ...state,
          threadId,
          // 新 thread 清空旧消息
          items: [],
          activeTurnId: null,
          status: "idle",
        };
      }
      case "turn/started": {
        const turn = (params as Record<string, unknown>)?.["turn"] as Record<string, unknown> | undefined;
        const turnId =
          (turn?.["id"] as string | undefined) ??
          (params as Record<string, unknown>)?.["turnId"] as string | undefined;
        const threadId = (params as Record<string, unknown>)?.["threadId"] as string | undefined;
        // 若事件 threadId 与当前视图不一致，且已有 threadId，则忽略或更新
        if (threadId && state.threadId && threadId !== state.threadId) {
          // 忽略跨 thread 的 turn 事件
          return state;
        }
        return {
          ...state,
          ...(threadId ? { threadId } : {}),
          activeTurnId: turnId ?? null,
          status: "running",
        };
      }
      case "item/started": {
        const item = (params as Record<string, unknown>)?.["item"] as unknown;
        if (!item) return state;
        const view = threadItemToView(item);
        // 去重：同 id 已存在则不重复添加
        if (state.items.some((it) => it.id === view.id)) return state;
        return { ...state, items: [...state.items, view] };
      }
      case "item/agentMessage/delta": {
        const itemId = (params as Record<string, unknown>)?.["itemId"] as string | undefined;
        const delta = (params as Record<string, unknown>)?.["delta"] as string | undefined;
        if (!itemId || typeof delta !== "string") return state;
        return {
          ...state,
          items: updateItemById(state.items, itemId, (prev) => {
            if (prev.kind === "agentMessage") {
              return { ...prev, text: prev.text + delta };
            }
            // 若未找到对应 agentMessage，创建一个增量项（兜底）
            if (prev.kind === "unknown") return prev;
            return prev;
          }),
        };
      }
      case "item/reasoning/summaryTextDelta":
      case "item/reasoning/textDelta": {
        const itemId = (params as Record<string, unknown>)?.["itemId"] as string | undefined;
        const delta = (params as Record<string, unknown>)?.["delta"] as string | undefined;
        if (!itemId || typeof delta !== "string") return state;
        return {
          ...state,
          items: updateItemById(state.items, itemId, (prev) => {
            if (prev.kind === "reasoning") {
              return { ...prev, text: prev.text + delta };
            }
            return prev;
          }),
        };
      }
      case "item/reasoning/summaryPartAdded": {
        // 仅占位：保证不抛错
        return state;
      }
      case "item/commandExecution/outputDelta":
      case "command/exec/outputDelta": {
        const itemId = (params as Record<string, unknown>)?.["itemId"] as string | undefined;
        const delta = (params as Record<string, unknown>)?.["delta"] as string | undefined;
        if (!itemId || typeof delta !== "string") return state;
        return {
          ...state,
          items: updateItemById(state.items, itemId, (prev) => {
            if (prev.kind === "commandExecution") {
              return { ...prev, output: prev.output + delta };
            }
            return prev;
          }),
        };
      }
      case "item/fileChange/outputDelta":
      case "item/fileChange/patchUpdated": {
        const itemId = (params as Record<string, unknown>)?.["itemId"] as string | undefined;
        const delta = (params as Record<string, unknown>)?.["delta"] as string | undefined;
        const patch = (params as Record<string, unknown>)?.["patch"] as string | undefined;
        if (!itemId) return state;
        // patchUpdated 可能带 patch 文本，视为 output 累积或忽略
        const text = delta ?? patch;
        if (typeof text !== "string") return state;
        return {
          ...state,
          items: updateItemById(state.items, itemId, (prev) => {
            if (prev.kind === "fileChange") {
              // 追加到第一个文件的 diff（简化）
              if (prev.files.length > 0) {
                const nextFiles = [...prev.files];
                nextFiles[0] = { ...nextFiles[0], diff: nextFiles[0].diff + text };
                return { ...prev, files: nextFiles };
              }
              return prev;
            }
            if (prev.kind === "commandExecution") {
              return { ...prev, output: prev.output + text };
            }
            return prev;
          }),
        };
      }
      case "item/plan/delta": {
        const itemId = (params as Record<string, unknown>)?.["itemId"] as string | undefined;
        const delta = (params as Record<string, unknown>)?.["delta"] as string | undefined;
        if (!itemId || typeof delta !== "string") return state;
        return {
          ...state,
          items: updateItemById(state.items, itemId, (prev) => {
            if (prev.kind === "plan") {
              return { ...prev, text: prev.text + delta };
            }
            return prev;
          }),
        };
      }
      case "item/completed": {
        const item = (params as Record<string, unknown>)?.["item"] as unknown;
        if (!item) return state;
        const view = threadItemToView(item);
        // 定型：标记 complete
        let finalized: ItemView = view;
        if (view.kind === "agentMessage") {
          finalized = { ...view, complete: true };
        } else if (view.kind === "reasoning") {
          finalized = { ...view, complete: true };
        }
        // 若已存在则替换，否则追加
        return { ...state, items: upsertItem(state.items, finalized) };
      }
      case "turn/completed": {
        const turn = (params as Record<string, unknown>)?.["turn"] as Record<string, unknown> | undefined;
        const turnId =
          (turn?.["id"] as string | undefined) ??
          (params as Record<string, unknown>)?.["turnId"] as string | undefined;
        // 仅当完成的 turn 是活跃 turn 时清空
        if (turnId && state.activeTurnId && turnId !== state.activeTurnId) {
          // 忽略非活跃 turn 的完成
          return state;
        }
        // 将未完成的 streaming 项标记为完成
        const finalizedItems = state.items.map((it) => {
          if (it.kind === "agentMessage" && !it.complete) return { ...it, complete: true };
          if (it.kind === "reasoning" && !it.complete) return { ...it, complete: true } as ItemView;
          if (it.kind === "commandExecution" && it.status === "inProgress")
            return { ...it, status: "completed" } as ItemView;
          if (it.kind === "fileChange" && it.status === "inProgress")
            return { ...it, status: "completed" } as ItemView;
          return it;
        });
        return {
          ...state,
          items: finalizedItems,
          activeTurnId: null,
          status: "idle",
        };
      }
      case "thread/tokenUsage/updated": {
        const usage = (params as Record<string, unknown>)?.["tokenUsage"] as unknown;
        return { ...state, tokenUsage: usage ?? null };
      }
      case "fs/changed": {
        // vault 一致性需要感知文件变更，状态本身不变，由 AgentPanel 侧收紧 refresh 条件
        return state;
      }
      case "turn/diff/updated": {
        const diff = (params as Record<string, unknown>)?.["diff"] as string | undefined;
        if (typeof diff !== "string" || !diff) return state;
        // 将 turn 维度的聚合 diff 合并到最近一个 fileChange 项的 diff 尾部（若存在），便于 MessageStream 预览
        // 若不存在 fileChange，则忽略（不新增 unknown）
        if (state.items.length === 0) return state;
        // 找最后一个 fileChange
        let idx = -1;
        for (let i = state.items.length - 1; i >= 0; i--) {
          if (state.items[i].kind === "fileChange") { idx = i; break; }
        }
        if (idx === -1) return state;
        const next = [...state.items];
        const prev = next[idx] as Extract<typeof next[number], { kind: "fileChange" }>;
        // 避免重复追加：若 diff 已是后缀则跳过
        if (prev.files.length > 0 && prev.files[0].diff.endsWith(diff)) return state;
        const files = [...prev.files];
        if (files.length > 0) {
          files[0] = { ...files[0], diff: (files[0].diff ? files[0].diff + "\n" : "") + diff };
        }
        (next as unknown as Array<Record<string, unknown>>)[idx] = { ...prev, files } as unknown as Record<string, unknown>;
        // 归一化回 ItemView
        return { ...state, items: next as typeof state.items };
      }
      case "turn/plan/updated": {
        const plan = (params as Record<string, unknown>)?.["plan"] as unknown;
        const explanation = (params as Record<string, unknown>)?.["explanation"] as string | undefined;
        let text = "";
        if (Array.isArray(plan)) {
          text = plan.map((step: unknown) => {
            const s = step as Record<string, unknown>;
            const title = typeof s["title"] === "string" ? (s["title"] as string) : "";
            const status = typeof s["status"] === "string" ? (s["status"] as string) : "";
            const stepText = typeof s["text"] === "string" ? (s["text"] as string) : "";
            const label = title || stepText || JSON.stringify(s);
            const box = status === "completed" ? "[x]" : "[ ]";
            return `- ${box} ${label}`;
          }).join("\n");
        }
        if (explanation) {
          text = (text ? text + "\n\n" : "") + explanation;
        }
        if (!text) return state;
        // 以固定 id 聚合 plan：若已存在则追加/替换，否则新增
        const planId = "__turn-plan__";
        const existing = state.items.find((it) => it.id === planId && it.kind === "plan");
        if (existing) {
          return {
            ...state,
            items: state.items.map((it) => (it.id === planId && it.kind === "plan" ? { ...it, text } : it)),
          };
        }
        return {
          ...state,
          items: [...state.items, { kind: "plan" as const, id: planId, text }],
        };
      }
      default: {
        // 未知 method 兜底：不抛错，返回原状态
        // 为可视化调试，可将未知事件作为 unknown 项追加（可选），M1 保持原样
        return state;
      }
    }
  } catch {
    // 任何异常兜底不抛错
    return state;
  }
}
