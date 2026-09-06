import { describe, it, expect } from "vitest";
import {
  createInitialAuthFlowState,
  isChatGptAuthenticated,
  interpretPollResult,
  interpretLoginCompletedNotification,
  shouldTimeout,
  isTerminal,
  reduceAuthFlow,
  shouldContinuePolling,
  CHAT_LOGIN_MAX_ATTEMPTS,
  CHAT_LOGIN_MAX_DURATION_MS,
} from "./authflow";
import type { GetAuthStatusResponse } from "./types/GetAuthStatusResponse";
import type { AccountLoginCompletedNotification } from "./types/v2/AccountLoginCompletedNotification";

function resp(overrides: Partial<GetAuthStatusResponse>): GetAuthStatusResponse {
  return {
    authMethod: null,
    authToken: null,
    requiresOpenaiAuth: null,
    ...overrides,
  } as GetAuthStatusResponse;
}

function notif(overrides: Partial<AccountLoginCompletedNotification>): AccountLoginCompletedNotification {
  return {
    loginId: null,
    success: true,
    error: null,
    onboardingEntrypoint: null,
    ...overrides,
  } as AccountLoginCompletedNotification;
}

describe("authflow 纯函数", () => {
  it("成功：requiresOpenaiAuth=false 视为已登录", () => {
    expect(isChatGptAuthenticated(resp({ requiresOpenaiAuth: false }))).toBe(true);
    expect(interpretPollResult(resp({ requiresOpenaiAuth: false }))).toBe("success");
  });

  it("pending：requiresOpenaiAuth=true / null", () => {
    expect(isChatGptAuthenticated(resp({ requiresOpenaiAuth: true }))).toBe(false);
    expect(isChatGptAuthenticated(resp({ requiresOpenaiAuth: null }))).toBe(false);
    expect(interpretPollResult(resp({ requiresOpenaiAuth: null }))).toBe("pending");
  });

  it("notification：success/false → success/failed", () => {
    expect(interpretLoginCompletedNotification(notif({ success: true }))).toBe("success");
    expect(interpretLoginCompletedNotification(notif({ success: false }))).toBe("failed");
  });

  it("timeout：按尝试次数与时长阈值", () => {
    expect(shouldTimeout(CHAT_LOGIN_MAX_ATTEMPTS, 0)).toBe(true);
    expect(shouldTimeout(CHAT_LOGIN_MAX_ATTEMPTS - 1, CHAT_LOGIN_MAX_DURATION_MS - 1)).toBe(false);
    expect(shouldTimeout(0, CHAT_LOGIN_MAX_DURATION_MS)).toBe(true);
    expect(shouldContinuePolling(0, 0)).toBe(true);
    expect(shouldContinuePolling(CHAT_LOGIN_MAX_ATTEMPTS, 0)).toBe(false);
  });

  it("isTerminal：仅 pending 非终态", () => {
    expect(isTerminal("pending")).toBe(false);
    expect(isTerminal("success")).toBe(true);
    expect(isTerminal("failed")).toBe(true);
    expect(isTerminal("timeout")).toBe(true);
  });

  it("reduceAuthFlow：poll 成功立即终结", () => {
    let s = createInitialAuthFlowState("lid-1");
    s = reduceAuthFlow(s, { type: "poll", resp: resp({ requiresOpenaiAuth: true }) });
    expect(s.status).toBe("pending");
    s = reduceAuthFlow(s, { type: "poll", resp: resp({ requiresOpenaiAuth: false }) });
    expect(s.status).toBe("success");
    // 已终态不再变化
    const frozen = s;
    s = reduceAuthFlow(s, { type: "poll", resp: resp({ requiresOpenaiAuth: true }) });
    expect(s).toBe(frozen);
  });

  it("reduceAuthFlow：notification 失败携带 error，timeout/error 也终结", () => {
    let s = createInitialAuthFlowState("lid-2");
    s = reduceAuthFlow(s, { type: "notification", notif: notif({ success: false, error: "cancelled" }) });
    expect(s).toMatchObject({ status: "failed", error: "cancelled" });

    let s2 = createInitialAuthFlowState();
    s2 = reduceAuthFlow(s2, { type: "timeout" });
    expect(s2.status).toBe("timeout");

    let s3 = createInitialAuthFlowState();
    s3 = reduceAuthFlow(s3, { type: "error", error: "network" });
    expect(s3).toMatchObject({ status: "failed", error: "network" });
  });
});
