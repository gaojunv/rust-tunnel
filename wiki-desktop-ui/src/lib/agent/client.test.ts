import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

const mockInvoke = vi.fn();
const mockListen = vi.fn(async () => () => {});

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mockInvoke(...(args as [string, unknown])),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: (...args: unknown[]) => mockListen(...(args as [string, unknown])),
}));

describe("agent client", () => {
  let client: typeof import("./client");

  beforeEach(async () => {
    mockInvoke.mockReset();
    mockListen.mockReset();
    mockListen.mockImplementation(async () => () => {});
    vi.useRealTimers();
    (globalThis as unknown as Record<string, unknown>).window = {
      __TAURI_INTERNALS__: {},
    } as unknown as Window;
    vi.resetModules();
    client = await import("./client");
  });

  afterEach(() => {
    vi.useRealTimers();
    client.__resetSleep();
    vi.restoreAllMocks();
  });

  it("agentRequest 方法名/参数映射与结果透传", async () => {
    mockInvoke.mockResolvedValue({ ok: 1 });
    const res = await client.agentRequest<{ ok: number }>("thread/start", { model: "m" });
    expect(mockInvoke).toHaveBeenCalledWith("agent_request", {
      method: "thread/start",
      params: { model: "m" },
    });
    expect(res).toEqual({ ok: 1 });
  });

  it("错误传播：非 -32001 直接 throw", async () => {
    mockInvoke.mockRejectedValue({ code: -32603, message: "internal" });
    await expect(client.agentRequest("turn/start", {})).rejects.toEqual({
      code: -32603,
      message: "internal",
    });
    expect(mockInvoke).toHaveBeenCalledTimes(1);
  });

  it("-32001 仅对幂等方法重试，次数与退避正确（可注入 sleep）", async () => {
    const backpressure = { code: -32001, message: "backpressure" };
    mockInvoke
      .mockRejectedValueOnce(backpressure)
      .mockRejectedValueOnce(backpressure)
      .mockRejectedValueOnce(backpressure)
      .mockRejectedValueOnce(backpressure)
      .mockRejectedValueOnce(backpressure)
      .mockResolvedValueOnce({ data: [] });

    const delays: number[] = [];
    client.__setSleep(async (ms: number) => {
      delays.push(ms);
    });

    const res = await client.agentRequest("thread/list", {});
    expect(res).toEqual({ data: [] });
    expect(mockInvoke).toHaveBeenCalledTimes(6);
    expect(delays).toEqual([200, 400, 800, 1600, 3200]);
  });

  it("-32001 非幂等方法不重试", async () => {
    const delays: number[] = [];
    client.__setSleep(async (ms: number) => {
      delays.push(ms);
    });
    mockInvoke.mockRejectedValue({ code: -32001, message: "backpressure" });
    await expect(client.agentRequest("turn/start", {})).rejects.toEqual({
      code: -32001,
      message: "backpressure",
    });
    expect(mockInvoke).toHaveBeenCalledTimes(1);
    expect(delays).toEqual([]);
  });

  it("-32001 超过 5 次后抛错（幂等方法）", async () => {
    const delays: number[] = [];
    client.__setSleep(async (ms: number) => {
      delays.push(ms);
    });
    mockInvoke.mockRejectedValue({ code: -32001, message: "backpressure" });
    await expect(client.agentRequest("thread/list", {})).rejects.toEqual({
      code: -32001,
      message: "backpressure",
    });
    expect(mockInvoke).toHaveBeenCalledTimes(6);
    expect(delays).toEqual([200, 400, 800, 1600, 3200]);
  });

  it("浏览器环境抛仅桌面端可用", async () => {
    (globalThis as unknown as Record<string, unknown>).window = {} as unknown as Window;
    vi.resetModules();
    const fresh = await import("./client");
    await expect(fresh.agentRequest("thread/list", {})).rejects.toThrow("仅桌面端可用");
    await expect(fresh.getStatus()).rejects.toThrow("仅桌面端可用");
    const unlisten = await fresh.onAgentNotification(() => {});
    expect(typeof unlisten).toBe("function");
  });

  it("typed wrappers 映射正确", async () => {
    mockInvoke.mockResolvedValue({
      thread: { id: "t1" },
      model: "m",
      modelProvider: "p",
      cwd: "/",
      instructionSources: [],
      approvalPolicy: "on-request",
      approvalsReviewer: "user",
      sandbox: {},
      reasoningEffort: null,
    });
    await client.threadStart({});
    expect(mockInvoke).toHaveBeenLastCalledWith("agent_request", {
      method: "thread/start",
      params: {},
    });
    mockInvoke.mockResolvedValue({ turn: { id: "turn1" } });
    await client.turnStart({ threadId: "t1", input: [] });
    expect(mockInvoke).toHaveBeenLastCalledWith("agent_request", {
      method: "turn/start",
      params: { threadId: "t1", input: [] },
    });
  });

  it("saveSettings 参数名映射为 {settings: dto}", async () => {
    mockInvoke.mockResolvedValue(undefined);
    const dto = {
      enabled: true,
      authMode: "gateway",
      gatewayBaseUrl: "https://example.com",
      gatewayApiKey: "k",
      openaiApiKey: null,
      model: "gpt-4o",
      approvalPolicy: "on-request",
      sandboxMode: "workspace-write",
      codexPathOverride: null,
    } as unknown as Parameters<typeof client.saveSettings>[0];
    await client.saveSettings(dto);
    expect(mockInvoke).toHaveBeenCalledWith("agent_save_settings", { settings: dto });
  });

  it("accountLoginStart/getAuthStatus 映射正确", async () => {
    mockInvoke.mockResolvedValue({ type: "chatgpt", loginId: "lid", authUrl: "https://example.com/auth" });
    const loginRes = await client.accountLoginStart({ type: "chatgpt" } as unknown as Parameters<
      typeof client.accountLoginStart
    >[0]);
    expect(mockInvoke).toHaveBeenCalledWith("agent_request", {
      method: "account/login/start",
      params: { type: "chatgpt" },
    });
    expect((loginRes as unknown as Record<string, unknown>)["authUrl"]).toBe("https://example.com/auth");

    mockInvoke.mockResolvedValue({ authMethod: "chatgpt", authToken: null, requiresOpenaiAuth: false });
    const authRes = await client.getAuthStatus({ includeToken: false, refreshToken: false });
    expect(mockInvoke).toHaveBeenCalledWith("agent_request", {
      method: "getAuthStatus",
      params: { includeToken: false, refreshToken: false },
    });
    expect(authRes.requiresOpenaiAuth).toBe(false);

    mockInvoke.mockResolvedValue([
      { id: "m1", model: "m1", displayName: "M1", hidden: false } as unknown as Record<string, unknown>,
    ]);
    // modelList 需 cursor 参数，传 null 也应透传
    mockInvoke.mockResolvedValue({ data: [{ id: "m1" }], nextCursor: null });
    const ml = await client.modelList({ cursor: null } as unknown as Parameters<typeof client.modelList>[0]);
    expect(mockInvoke).toHaveBeenCalledWith("agent_request", {
      method: "model/list",
      params: { cursor: null },
    });
    expect((ml as unknown as Record<string, unknown>)["data"]).toBeDefined();

    // openExternal
    mockInvoke.mockResolvedValue(undefined);
    await client.openExternal("https://example.com/auth");
    expect(mockInvoke).toHaveBeenCalledWith("agent_open_external", { url: "https://example.com/auth" });
  });

  it("getSettings/detectBinary/getStatus/start/stop", async () => {
    mockInvoke.mockResolvedValue({ enabled: false, authMode: "gateway" });
    const s = await client.getSettings();
    expect(mockInvoke).toHaveBeenCalledWith("agent_get_settings");
    expect((s as unknown as Record<string, unknown>)["authMode"]).toBe("gateway");

    mockInvoke.mockResolvedValue({ path: "/tmp/codex", argv_prefix: [], source: "path", version: "0.1.0" });
    const bin = await client.detectBinary();
    expect(mockInvoke).toHaveBeenCalledWith("agent_detect_binary");
    expect(bin?.path).toBe("/tmp/codex");

    mockInvoke.mockResolvedValue({ phase: "running", bin: "/tmp/codex" });
    const st = await client.getStatus();
    expect(mockInvoke).toHaveBeenCalledWith("agent_get_status");
    expect(st.phase).toBe("running");

    mockInvoke.mockResolvedValue({ phase: "running", bin: "/tmp/codex" });
    const st2 = await client.start();
    expect(mockInvoke).toHaveBeenCalledWith("agent_start");
    expect(st2.phase).toBe("running");

    mockInvoke.mockResolvedValue(undefined);
    await client.stop();
    expect(mockInvoke).toHaveBeenCalledWith("agent_stop");
  });
});
