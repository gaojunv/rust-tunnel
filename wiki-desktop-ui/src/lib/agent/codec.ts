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
  | { kind: "error"; id: string; message: string }
  | { kind: "system"; id: string; text: string; tone?: "info" | "warn" }
  | { kind: "unknown"; id: string; raw: unknown };

/** turn 级聚合 diff（`turn/diff/updated` 最新快照，按 turnId 存放） */
export type TurnDiffView = {
  turnId: string;
  diff: string;
};

export type AgentThreadView = {
  threadId: string;
  items: ItemView[];
  activeTurnId?: string | null;
  status: string;
  tokenUsage?: unknown;
  /** turn 聚合 diff 快照（2A' Diff 审查的数据源；与 fileChange item 解耦） */
  turnDiffs: TurnDiffView[];
  /** 账户限流快照（`account/rateLimits/updated` 原样存放，仅状态栏消费） */
  rateLimits?: unknown;
};

export type TokenUsageView = unknown;

export function createInitialView(threadId = ""): AgentThreadView {
  return {
    threadId,
    items: [],
    activeTurnId: null,
    status: "idle",
    tokenUsage: null,
    turnDiffs: [],
    rateLimits: null,
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

// —— 历史回填：Turn[] → 只读 ItemView[] ——

/**
 * 历史回填映射（2C「resume 旧会话」数据源）。
 * 将 `thread/turns/list` 返回的历史 Turn 按 turn 顺序、turn 内 item 顺序展开为只读 ItemView；
 * 复用 threadItemToView，无法还原的类型（hookPrompt/mcpToolCall 等 → unknown）一律跳过；
 * 畸形条目容错（跳过不抛错）。
 *
 * 输入用 codec 一贯的宽松结构类型（避免在此 import vendor Turn；vendor Turn 天然可赋值）。
 */
export type HydratableTurn = {
  id?: string | null;
  items?: Array<Record<string, unknown>> | null;
};

export function hydrateTurnsToItems(turns: readonly HydratableTurn[]): ItemView[] {
  if (!Array.isArray(turns)) return [];
  const out: ItemView[] = [];
  for (const turn of turns) {
    if (!turn || typeof turn !== "object") continue; // 畸形 turn：跳过
    const items = (turn as { items?: unknown }).items;
    if (!Array.isArray(items)) continue;
    for (const raw of items) {
      try {
        if (raw == null || typeof raw !== "object") continue; // 畸形 item：跳过
        const view = threadItemToView(raw);
        if (view.kind === "unknown") continue; // 无法还原的类型：跳过/降级
        out.push(view);
      } catch {
        // 容错：单项异常不影响整段回填
        continue;
      }
    }
  }
  return out;
}

// —— error 提取 ——

/**
 * 从各类 error 形态中提取可展示文本（纯函数）：
 * - 字符串：直接使用
 * - TurnError：`{ message, additionalDetails, ... }`
 * - 其余对象：尝试 `message` 字段
 */
function extractErrorText(value: unknown): string | null {
  if (value == null) return null;
  if (typeof value === "string") {
    const t = value.trim();
    return t || null;
  }
  if (typeof value === "object") {
    const o = value as Record<string, unknown>;
    const msg = o["message"];
    if (typeof msg === "string" && msg.trim()) {
      const add = o["additionalDetails"];
      const detail = typeof add === "string" && add.trim() ? add.trim() : "";
      return detail ? `${msg.trim()}\n${detail}` : msg.trim();
    }
  }
  return null;
}

/** 生成（或复用）一条 error item；已存在同 id 的 error 则不重复追加 */
function appendErrorItem(items: ItemView[], id: string, message: string): ItemView[] {
  if (items.some((it) => it.kind === "error" && it.id === id)) return items;
  return [...items, { kind: "error", id, message }];
}

// —— system 通知 ——

/** upsert 一条 system item；同 id 已存在则替换（去重），否则追加 */
function upsertSystemItem(
  items: ItemView[],
  id: string,
  text: string,
  tone: "info" | "warn",
): ItemView[] {
  const idx = items.findIndex((it) => it.kind === "system" && it.id === id);
  if (idx === -1) return [...items, { kind: "system", id, text, tone }];
  const next = [...items];
  next[idx] = { kind: "system", id, text, tone };
  return next;
}

/** model/rerouted 原因枚举转中文（未知取值回退原文） */
function rerouteReasonText(reason: unknown): string {
  if (reason === "highRiskCyberActivity") return "高风险网络活动";
  return typeof reason === "string" && reason ? reason : "未知原因";
}

/**
 * 将 system 类通知映射为 { id, text, tone }；无法构造文案时返回 null。
 * 幂等：同 id 且内容一致的通知到达时由调用方跳过，避免消息流重复刷屏。
 */
function systemItemFromEvent(
  method: string,
  params: Record<string, unknown>,
): { id: string; text: string; tone: "info" | "warn" } | null {
  const detail =
    typeof params["details"] === "string" && params["details"]
      ? (params["details"] as string)
      : "";
  const turnId = typeof params["turnId"] === "string" ? (params["turnId"] as string) : null;
  const textOf = (key: "message" | "summary"): string =>
    typeof params[key] === "string" && params[key] ? (params[key] as string) : "";

  if (method === "warning") {
    const text = textOf("message");
    if (!text) return null;
    const threadId = typeof params["threadId"] === "string" ? (params["threadId"] as string) : null;
    const body = detail ? `${text}\n${detail}` : text;
    return { id: threadId ? `system-warning-${threadId}` : "system-warning", text: body, tone: "warn" };
  }
  if (method === "configWarning") {
    const text = textOf("summary");
    if (!text) return null;
    const path =
      typeof params["path"] === "string" && params["path"] ? (params["path"] as string) : "";
    const body = text + (path ? `（${path}）` : "") + (detail ? `\n${detail}` : "");
    return { id: "system-configWarning", text: body, tone: "warn" };
  }
  if (method === "deprecationNotice") {
    const text = textOf("summary");
    if (!text) return null;
    const body = detail ? `${text}\n${detail}` : text;
    return { id: "system-deprecationNotice", text: body, tone: "warn" };
  }
  if (method === "model/rerouted") {
    const fromModel =
      typeof params["fromModel"] === "string" && params["fromModel"]
        ? (params["fromModel"] as string)
        : "原模型";
    const toModel =
      typeof params["toModel"] === "string" && params["toModel"]
        ? (params["toModel"] as string)
        : "新模型";
    const body = `模型已从 ${fromModel} 切换为 ${toModel}：${rerouteReasonText(params["reason"])}`;
    return { id: `system-model-rerouted-${turnId ?? "turn"}`, text: body, tone: "info" };
  }
  if (method === "thread/compacted") {
    return {
      id: `system-thread-compacted-${turnId ?? "thread"}`,
      text: "对话上下文已压缩整理，更早内容保留为摘要",
      tone: "info",
    };
  }
  return null;
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
          // 新 thread 清空旧消息与 turn 聚合 diff（rateLimits 为账户级，保留）
          items: [],
          turnDiffs: [],
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
        const paramsObj = (params as Record<string, unknown>) ?? {};
        const turn = paramsObj["turn"] as Record<string, unknown> | undefined;
        const turnId =
          (turn?.["id"] as string | undefined) ??
          (paramsObj["turnId"] as string | undefined);
        // 仅当完成的 turn 是活跃 turn 时清空
        if (turnId && state.activeTurnId && turnId !== state.activeTurnId) {
          // 忽略非活跃 turn 的完成
          return state;
        }
        // 将未完成的 streaming 项标记为完成
        let finalizedItems = state.items.map((it) => {
          if (it.kind === "agentMessage" && !it.complete) return { ...it, complete: true };
          if (it.kind === "reasoning" && !it.complete) return { ...it, complete: true } as ItemView;
          if (it.kind === "commandExecution" && it.status === "inProgress")
            return { ...it, status: "completed" } as ItemView;
          if (it.kind === "fileChange" && it.status === "inProgress")
            return { ...it, status: "completed" } as ItemView;
          return it;
        });
        // 若 turn 携带 error（TurnError 或 params.error 顶层字段），追加 error item 而非静默收尾
        const errValue = paramsObj["error"] ?? turn?.["error"];
        const errMsg = extractErrorText(errValue);
        if (errMsg) {
          finalizedItems = appendErrorItem(
            finalizedItems,
            `error-${turnId ?? "turn"}`,
            errMsg,
          );
        }
        return {
          ...state,
          items: finalizedItems,
          activeTurnId: null,
          status: "idle",
        };
      }
      case "error": {
        // ACP 顶层 error 通知（ErrorNotification: { error: TurnError, turnId, ... }）
        const paramsObj = (params as Record<string, unknown>) ?? {};
        const errValue = paramsObj["error"];
        const errMsg = extractErrorText(errValue);
        if (!errMsg) return state;
        const turnId = paramsObj["turnId"] as string | undefined;
        return {
          ...state,
          items: appendErrorItem(state.items, `error-${turnId ?? "notification"}`, errMsg),
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
      case "account/rateLimits/updated": {
        // 稀疏限流更新：原样存放快照供状态栏 title 提示，不刷消息流（2D）
        const rl = (params as Record<string, unknown>)?.["rateLimits"] as unknown;
        if (rl == null) return state;
        return { ...state, rateLimits: rl };
      }
      case "warning":
      case "configWarning":
      case "model/rerouted":
      case "thread/compacted":
      case "deprecationNotice": {
        // system 通知 → 消息流 system item（此前被静默吞掉，不再丢弃）
        const sys = systemItemFromEvent(method, (params as Record<string, unknown>) ?? {});
        if (!sys) return state;
        const existing = state.items.find((it) => it.kind === "system" && it.id === sys.id);
        // 同 id 且内容一致：不产生新状态（幂等去重）
        if (existing && existing.kind === "system" && existing.text === sys.text) return state;
        return { ...state, items: upsertSystemItem(state.items, sys.id, sys.text, sys.tone) };
      }
      case "turn/diff/updated": {
        // turn 级聚合 diff 按 turnId 存入 turnDiffs（与 fileChange item 解耦；
        // 旧 hack「append 到最后一个 fileChange」已迁移删除：MessageStream 仅用
        // fileChange 自身 diff 做预览，删除后既有展示不受影响）。
        // 通知携带的是该 turn 最新全量快照，同 turnId 重复到达时替换（内容一致则直接返回原状态）。
        const paramsObj = (params as Record<string, unknown>) ?? {};
        const diff = paramsObj["diff"] as string | undefined;
        const turnId =
          (paramsObj["turnId"] as string | undefined) ?? state.activeTurnId ?? undefined;
        if (typeof diff !== "string" || !diff || !turnId) return state;
        const idx = state.turnDiffs.findIndex((t) => t.turnId === turnId);
        if (idx !== -1) {
          if (state.turnDiffs[idx].diff === diff) return state;
          const next = [...state.turnDiffs];
          next[idx] = { turnId, diff };
          return { ...state, turnDiffs: next };
        }
        return { ...state, turnDiffs: [...state.turnDiffs, { turnId, diff }] };
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
