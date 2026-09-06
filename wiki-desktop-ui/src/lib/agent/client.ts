/**
 * Codex agent 协议语义层 —— 唯一 import ./types 的地方
 * 封装 Tauri commands/events，提供 typed wrappers 与 -32001 背压重试
 */
import type {
  ServerNotification,
  ServerRequest,
  ThreadStartParams,
  ThreadStartResponse,
  ThreadListParams,
  ThreadListResponse,
  ThreadResumeParams,
  ThreadResumeResponse,
} from "./types";
import type { ThreadReadParams } from "./types/v2/ThreadReadParams";
import type { ThreadArchiveParams } from "./types/v2/ThreadArchiveParams";
import type { ThreadArchiveResponse } from "./types/v2/ThreadArchiveResponse";
import type { ThreadReadResponse } from "./types/v2/ThreadReadResponse";
import type { TurnStartParams } from "./types/v2/TurnStartParams";
import type { TurnStartResponse } from "./types/v2/TurnStartResponse";
import type { TurnInterruptParams } from "./types/v2/TurnInterruptParams";
import type { TurnInterruptResponse } from "./types/v2/TurnInterruptResponse";
import type { TurnSteerParams } from "./types/v2/TurnSteerParams";
import type { TurnSteerResponse } from "./types/v2/TurnSteerResponse";
import type { ModelListParams } from "./types/v2/ModelListParams";
import type { ModelListResponse } from "./types/v2/ModelListResponse";
import type { GetAuthStatusParams } from "./types/GetAuthStatusParams";
import type { GetAuthStatusResponse } from "./types/GetAuthStatusResponse";
import type { FuzzyFileSearchParams } from "./types/FuzzyFileSearchParams";
import type { FuzzyFileSearchResponse } from "./types/FuzzyFileSearchResponse";
import type { LoginAccountParams } from "./types/v2/LoginAccountParams";
import type { LoginAccountResponse } from "./types/v2/LoginAccountResponse";

// 仅桌面端可用错误文案，与现有中文 UI 一致
const DESKTOP_ONLY_MSG = "仅桌面端可用";

// 幂等只读方法集合：-32001 背压时允许重试
const IDEMPOTENT_METHODS = new Set<string>([
  "thread/list",
  "model/list",
  "getAuthStatus",
  "fuzzyFileSearch",
  "thread/read",
  "thread/turns/list",
  "thread/items/list",
  "thread/loaded/list",
  "config/read",
]);

export type UnlistenFn = () => void;

export type AgentStatusDto = {
  phase: "stopped" | "starting" | "running" | "exited" | string;
  bin?: string;
  version?: string | null;
  code?: number | null;
  stderr_tail?: string[];
  reason?: string;
  // 兼容后端契约可能扩展的字段
  binary?: { path: string; source: string; version?: string | null };
  codexHome?: string;
  pid?: number | null;
  lastError?: string | null;
  [key: string]: unknown;
};

export type BinaryResolveDto = {
  path: string;
  argv_prefix: string[];
  source: string;
  version?: string | null;
};

export type AgentSettingsDto = {
  enabled: boolean;
  authMode: string;
  gatewayBaseUrl?: string | null;
  gatewayApiKey?: string | null;
  openaiApiKey?: string | null;
  model?: string | null;
  approvalPolicy: string;
  sandboxMode: string;
  codexPathOverride?: string | null;
  [key: string]: unknown;
};

function isTauriEnv(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

let sleepFn: (ms: number) => Promise<void> = (ms: number) =>
  new Promise((resolve) => {
    setTimeout(resolve, ms);
  });

// 测试可注入（避免 fake timers 与 dynamic import 冲突）
export function __setSleep(fn: (ms: number) => Promise<void>): void {
  sleepFn = fn;
}

export function __resetSleep(): void {
  sleepFn = (ms: number) =>
    new Promise((resolve) => {
      setTimeout(resolve, ms);
    });
}

function sleep(ms: number): Promise<void> {
  return sleepFn(ms);
}

function extractCode(err: unknown): number | null {
  if (err == null) return null;
  // 直接对象
  if (typeof err === "object") {
    const o = err as Record<string, unknown>;
    if (typeof o["code"] === "number") return o["code"] as number;
    if (o["error"] && typeof o["error"] === "object") {
      const inner = o["error"] as Record<string, unknown>;
      if (typeof inner["code"] === "number") return inner["code"] as number;
    }
    // Tauri 可能把错误序列化为字符串放在 message
    if (typeof o["message"] === "string") {
      const m = o["message"] as string;
      if (m.includes("-32001")) return -32001;
      try {
        const parsed = JSON.parse(m) as Record<string, unknown>;
        if (typeof parsed["code"] === "number") return parsed["code"] as number;
        if (parsed["error"] && typeof parsed["error"] === "object") {
          const e2 = parsed["error"] as Record<string, unknown>;
          if (typeof e2["code"] === "number") return e2["code"] as number;
        }
      } catch {
        // ignore
      }
    }
  }
  if (typeof err === "string") {
    if (err.includes("-32001")) return -32001;
    try {
      const parsed = JSON.parse(err) as Record<string, unknown>;
      if (typeof parsed["code"] === "number") return parsed["code"] as number;
      if (parsed["error"] && typeof parsed["error"] === "object") {
        const e2 = parsed["error"] as Record<string, unknown>;
        if (typeof e2["code"] === "number") return e2["code"] as number;
      }
    } catch {
      // ignore
    }
    // 形如 "agent 错误：{\"code\":-32001,...}"
    const match = err.match(/"code"\s*:\s*(-?\d+)/);
    if (match) {
      const n = Number(match[1]);
      if (Number.isFinite(n)) return n;
    }
  }
  return null;
}

/**
 * 泛型 JSON-RPC 调用封装
 * - 仅桌面端可用时抛错，保证浏览器 `npm run dev` 不崩
 * - -32001 背压仅对幂等方法指数退避重试（200ms 起，×5 次）
 */
export async function agentRequest<T>(method: string, params: unknown): Promise<T> {
  if (!isTauriEnv()) {
    throw new Error(DESKTOP_ONLY_MSG);
  }
  let attempt = 0;
  let delay = 200;
  for (;;) {
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      // Rust 侧 agent_request 签名 (method: String, params: Option<Value>)
      const result = await invoke<T>("agent_request", {
        method,
        params: (params ?? null) as unknown,
      });
      return result;
    } catch (err: unknown) {
      const code = extractCode(err);
      if (code === -32001 && IDEMPOTENT_METHODS.has(method) && attempt < 5) {
        attempt += 1;
        await sleep(delay);
        delay *= 2;
        continue;
      }
      throw err;
    }
  }
}

// —— typed wrappers ——

export function threadStart(params: ThreadStartParams): Promise<ThreadStartResponse> {
  return agentRequest<ThreadStartResponse>("thread/start", params);
}

export function threadList(params: ThreadListParams): Promise<ThreadListResponse> {
  return agentRequest<ThreadListResponse>("thread/list", params);
}

export function threadResume(params: ThreadResumeParams): Promise<ThreadResumeResponse> {
  return agentRequest<ThreadResumeResponse>("thread/resume", params);
}

export function threadArchive(params: ThreadArchiveParams): Promise<ThreadArchiveResponse> {
  return agentRequest<ThreadArchiveResponse>("thread/archive", params);
}

export function threadRead(params: ThreadReadParams): Promise<ThreadReadResponse> {
  return agentRequest<ThreadReadResponse>("thread/read", params);
}

export function turnStart(params: TurnStartParams): Promise<TurnStartResponse> {
  return agentRequest<TurnStartResponse>("turn/start", params);
}

export function turnInterrupt(params: TurnInterruptParams): Promise<TurnInterruptResponse> {
  return agentRequest<TurnInterruptResponse>("turn/interrupt", params);
}

export function turnSteer(params: TurnSteerParams): Promise<TurnSteerResponse> {
  return agentRequest<TurnSteerResponse>("turn/steer", params);
}

export function modelList(params: ModelListParams): Promise<ModelListResponse> {
  return agentRequest<ModelListResponse>("model/list", params);
}

export function getAuthStatus(params: GetAuthStatusParams): Promise<GetAuthStatusResponse> {
  return agentRequest<GetAuthStatusResponse>("getAuthStatus", params);
}

export function fuzzyFileSearch(params: FuzzyFileSearchParams): Promise<FuzzyFileSearchResponse> {
  return agentRequest<FuzzyFileSearchResponse>("fuzzyFileSearch", params);
}

export function accountLoginStart(params: LoginAccountParams): Promise<LoginAccountResponse> {
  return agentRequest<LoginAccountResponse>("account/login/start", params);
}

export async function agentRespond(id: unknown, result?: unknown, error?: unknown): Promise<void> {
  if (!isTauriEnv()) throw new Error(DESKTOP_ONLY_MSG);
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("agent_respond", {
    id: id as unknown,
    result: (result ?? null) as unknown,
    error: (error ?? null) as unknown,
  });
}

// —— 事件订阅 ——

export async function onAgentNotification(
  cb: (n: ServerNotification) => void,
): Promise<UnlistenFn> {
  if (!isTauriEnv()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  const unlisten = await listen<ServerNotification | { method: string; params: unknown }>(
    "agent:notification",
    (event) => {
      const payload = event.payload as unknown as ServerNotification;
      cb(payload);
    },
  );
  return unlisten;
}

export async function onServerRequest(
  cb: (req: ServerRequest & { id: unknown }) => void,
): Promise<UnlistenFn> {
  if (!isTauriEnv()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  const unlisten = await listen<{ id: unknown; method: string; params: unknown }>(
    "agent:server-request",
    (event) => {
      const p = event.payload as { id: unknown; method: string; params: unknown };
      // 兼容 ServerRequest 联合类型的 method 字段
      cb(p as unknown as ServerRequest & { id: unknown });
    },
  );
  return unlisten;
}

export async function onAgentStatus(cb: (s: AgentStatusDto) => void): Promise<UnlistenFn> {
  if (!isTauriEnv()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  const unlisten = await listen<AgentStatusDto>("agent:status", (event) => {
    cb(event.payload as AgentStatusDto);
  });
  return unlisten;
}

// —— 状态与设置 ——

export async function getStatus(): Promise<AgentStatusDto> {
  if (!isTauriEnv()) throw new Error(DESKTOP_ONLY_MSG);
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<AgentStatusDto>("agent_get_status");
}

export async function start(): Promise<AgentStatusDto> {
  if (!isTauriEnv()) throw new Error(DESKTOP_ONLY_MSG);
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<AgentStatusDto>("agent_start");
}

export async function stop(): Promise<void> {
  if (!isTauriEnv()) throw new Error(DESKTOP_ONLY_MSG);
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("agent_stop");
}

export async function detectBinary(): Promise<BinaryResolveDto | null> {
  if (!isTauriEnv()) throw new Error(DESKTOP_ONLY_MSG);
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<BinaryResolveDto | null>("agent_detect_binary");
}

export async function getSettings(): Promise<AgentSettingsDto> {
  if (!isTauriEnv()) throw new Error(DESKTOP_ONLY_MSG);
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<AgentSettingsDto>("agent_get_settings");
}

export async function saveSettings(dto: AgentSettingsDto): Promise<void> {
  if (!isTauriEnv()) throw new Error(DESKTOP_ONLY_MSG);
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("agent_save_settings", { settings: dto as unknown as Record<string, unknown> });
}

export async function openExternal(url: string): Promise<void> {
  if (!isTauriEnv()) throw new Error(DESKTOP_ONLY_MSG);
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("agent_open_external", { url });
}
