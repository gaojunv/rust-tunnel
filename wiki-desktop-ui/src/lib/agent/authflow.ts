/**
 * 登录轮询状态机（纯函数）
 * 将 SettingsDialog 中的轮询/通知逻辑抽离，便于单测
 */
import type { GetAuthStatusResponse } from "./types/GetAuthStatusResponse";
import type { AccountLoginCompletedNotification } from "./types/v2/AccountLoginCompletedNotification";

export const CHAT_LOGIN_POLL_INTERVAL_MS = 2000;
export const CHAT_LOGIN_MAX_DURATION_MS = 120_000;
export const CHAT_LOGIN_MAX_ATTEMPTS = CHAT_LOGIN_MAX_DURATION_MS / CHAT_LOGIN_POLL_INTERVAL_MS;

export type AuthFlowStatus = "pending" | "success" | "failed" | "timeout";

export type AuthFlowState = {
  status: AuthFlowStatus;
  error?: string | null;
  loginId?: string | null;
};

export function createInitialAuthFlowState(loginId: string | null = null): AuthFlowState {
  return { status: "pending", error: null, loginId };
}

export function isChatGptAuthenticated(resp: GetAuthStatusResponse): boolean {
  // 后端契约：requiresOpenaiAuth === false 表示已认证；null 视为未定，仍 pending
  if (resp.requiresOpenaiAuth === false) return true;
  // 兼容：chatgpt 系列 authMethod 且不要求再次认证也视为成功
  if (resp.authMethod === "chatgpt" && resp.requiresOpenaiAuth !== true) {
    // 若 authMethod 已是 chatgpt，即便 requiresOpenaiAuth 为 null 也视作 pending，保守起见不误判
    // 但 codex 在已登录时通常会返回 requiresOpenaiAuth=false，故上面已覆盖
    return false;
  }
  if (resp.authMethod === "chatgptAuthTokens" && resp.requiresOpenaiAuth !== true) return false;
  return false;
}

export function interpretPollResult(resp: GetAuthStatusResponse): AuthFlowStatus {
  return isChatGptAuthenticated(resp) ? "success" : "pending";
}

export function interpretLoginCompletedNotification(
  notif: AccountLoginCompletedNotification,
): AuthFlowStatus {
  return notif.success ? "success" : "failed";
}

export function shouldTimeout(attempts: number, elapsedMs: number): boolean {
  return attempts >= CHAT_LOGIN_MAX_ATTEMPTS || elapsedMs >= CHAT_LOGIN_MAX_DURATION_MS;
}

export function isTerminal(status: AuthFlowStatus): boolean {
  return status !== "pending";
}

export function reduceAuthFlow(
  state: AuthFlowState,
  event:
    | { type: "poll"; resp: GetAuthStatusResponse }
    | { type: "notification"; notif: AccountLoginCompletedNotification }
    | { type: "timeout" }
    | { type: "error"; error: string },
): AuthFlowState {
  if (state.status !== "pending") return state;
  switch (event.type) {
    case "poll": {
      const s = interpretPollResult(event.resp);
      if (s === "success") return { ...state, status: "success" };
      return state;
    }
    case "notification": {
      const s = interpretLoginCompletedNotification(event.notif);
      if (s === "success") return { ...state, status: "success" };
      return { ...state, status: "failed", error: event.notif.error ?? null };
    }
    case "timeout":
      return { ...state, status: "timeout" };
    case "error":
      return { ...state, status: "failed", error: event.error };
    default:
      return state;
  }
}

/**
 * 纯函数：根据已过时间和尝试次数决定是否继续轮询
 */
export function shouldContinuePolling(attempts: number, elapsedMs: number): boolean {
  return !shouldTimeout(attempts, elapsedMs);
}
