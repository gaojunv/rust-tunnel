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
    const unlistenParse = await fresh.onParseError(() => {});
    expect(typeof unlistenParse).toBe("function");
  });

  it("onParseError 透传 agent:parse-error 载荷", async () => {
    const events: Array<{ event: string; cb: (e: { payload: unknown }) => void }> = [];
    mockListen.mockImplementation(async (event: string, cb: (e: { payload: unknown }) => void) => {
      events.push({ event, cb });
      return () => {};
    });
    const payload = { line: "not-json", error: "expected value at line 1 column 1" };
    let received: { line: string; error: string } | null = null;
    const unlisten = await client.onParseError((p) => {
      received = p;
    });
    expect(typeof unlisten).toBe("function");
    const sub = events.find((e) => e.event === "agent:parse-error");
    expect(sub).toBeDefined();
    sub?.cb({ payload });
    expect(received).toEqual(payload);
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

  it("新方法名/参数映射（threadRevert/setName/delete/unarchive/compact/review/turns/items/fs）", async () => {
    mockInvoke.mockResolvedValue({});
    await client.threadRevert({ threadId: "t1", beforeTurnId: "turn-9" });
    expect(mockInvoke).toHaveBeenLastCalledWith("agent_request", {
      method: "thread/revert",
      params: { threadId: "t1", beforeTurnId: "turn-9" },
    });
    await client.threadSetName({ threadId: "t1", name: "新会话" });
    expect(mockInvoke).toHaveBeenLastCalledWith("agent_request", {
      method: "thread/name/set",
      params: { threadId: "t1", name: "新会话" },
    });
    await client.threadDelete({ threadId: "t1" });
    expect(mockInvoke).toHaveBeenLastCalledWith("agent_request", {
      method: "thread/delete",
      params: { threadId: "t1" },
    });
    await client.threadUnarchive({ threadId: "t1" });
    expect(mockInvoke).toHaveBeenLastCalledWith("agent_request", {
      method: "thread/unarchive",
      params: { threadId: "t1" },
    });
    await client.threadCompactStart({ threadId: "t1" });
    expect(mockInvoke).toHaveBeenLastCalledWith("agent_request", {
      method: "thread/compact/start",
      params: { threadId: "t1" },
    });
    await client.reviewStart({ threadId: "t1", target: { type: "uncommittedChanges" } });
    expect(mockInvoke).toHaveBeenLastCalledWith("agent_request", {
      method: "review/start",
      params: { threadId: "t1", target: { type: "uncommittedChanges" } },
    });
    await client.threadTurnsList({ threadId: "t1", cursor: null, limit: 50, sortDirection: "asc", itemsView: "full" });
    expect(mockInvoke).toHaveBeenLastCalledWith("agent_request", {
      method: "thread/turns/list",
      params: { threadId: "t1", cursor: null, limit: 50, sortDirection: "asc", itemsView: "full" },
    });
    await client.threadItemsList({ threadId: "t1", turnId: "turn-1", cursor: null, limit: 50, sortDirection: "asc" });
    expect(mockInvoke).toHaveBeenLastCalledWith("agent_request", {
      method: "thread/items/list",
      params: { threadId: "t1", turnId: "turn-1", cursor: null, limit: 50, sortDirection: "asc" },
    });
    await client.fsReadFile({ path: "/vault/a.md" });
    expect(mockInvoke).toHaveBeenLastCalledWith("agent_request", {
      method: "fs/readFile",
      params: { path: "/vault/a.md" },
    });
    await client.fsWriteFile({ path: "/vault/a.md", dataBase64: "aGk=" });
    expect(mockInvoke).toHaveBeenLastCalledWith("agent_request", {
      method: "fs/writeFile",
      params: { path: "/vault/a.md", dataBase64: "aGk=" },
    });
  });

  it("fs/readFile 是幂等的（-32001 重试），fs/writeFile 不重试", async () => {
    const backpressure = { code: -32001, message: "backpressure" };
    client.__setSleep(async () => {});
    mockInvoke
      .mockRejectedValueOnce(backpressure)
      .mockResolvedValueOnce({ dataBase64: "aGk=" });
    const res = await client.fsReadFile({ path: "/vault/a.md" });
    expect(res).toEqual({ dataBase64: "aGk=" });
    expect(mockInvoke).toHaveBeenCalledTimes(2);

    mockInvoke.mockReset();
    mockInvoke.mockRejectedValue(backpressure);
    await expect(client.fsWriteFile({ path: "/vault/a.md", dataBase64: "eA==" })).rejects.toEqual(
      backpressure,
    );
    expect(mockInvoke).toHaveBeenCalledTimes(1);
  });

  it("写方法（threadRevert/threadSetName/threadDelete/unarchive/compact/review）-32001 不重试", async () => {
    const backpressure = { code: -32001, message: "backpressure" };
    const delays: number[] = [];
    client.__setSleep(async (ms: number) => {
      delays.push(ms);
    });
    mockInvoke.mockRejectedValue(backpressure);
    await expect(client.threadRevert({ threadId: "t1", beforeTurnId: "x" })).rejects.toEqual(backpressure);
    await expect(client.threadSetName({ threadId: "t1", name: "n" })).rejects.toEqual(backpressure);
    await expect(client.threadDelete({ threadId: "t1" })).rejects.toEqual(backpressure);
    await expect(client.threadUnarchive({ threadId: "t1" })).rejects.toEqual(backpressure);
    await expect(client.threadCompactStart({ threadId: "t1" })).rejects.toEqual(backpressure);
    await expect(
      client.reviewStart({ threadId: "t1", target: { type: "uncommittedChanges" } }),
    ).rejects.toEqual(backpressure);
    // 每个方法各调用 1 次（mockInvoke 累计），退避 sleep 从未触发
    expect(mockInvoke).toHaveBeenCalledTimes(6);
    expect(delays).toEqual([]);
  });

  it("thread/turns/list 与 thread/items/list 是幂等的（-32001 重试）", async () => {
    const backpressure = { code: -32001, message: "backpressure" };
    client.__setSleep(async () => {});
    mockInvoke
      .mockRejectedValueOnce(backpressure)
      .mockResolvedValueOnce({ data: [], nextCursor: null, backwardsCursor: null });
    const turns = await client.threadTurnsList({ threadId: "t1" });
    expect((turns as unknown as Record<string, unknown>)["data"]).toEqual([]);
    expect(mockInvoke).toHaveBeenCalledTimes(2);

    mockInvoke.mockReset();
    mockInvoke
      .mockRejectedValueOnce(backpressure)
      .mockResolvedValueOnce({ data: [], nextCursor: null, backwardsCursor: null });
    const items = await client.threadItemsList({ threadId: "t1" });
    expect((items as unknown as Record<string, unknown>)["data"]).toEqual([]);
    expect(mockInvoke).toHaveBeenCalledTimes(2);
  });

  it("base64 编解码 UTF-8 往返（中文/emoji）与 fsReadFileText", async () => {
    const text = "你好世界 ✓ emoji 🎉 mixed ASCII";
    const encoded = client.encodeUtf8ToBase64(text);
    expect(client.decodeBase64ToUtf8(encoded)).toBe(text);
    // 纯 ASCII 兼容
    expect(client.decodeBase64ToUtf8(client.encodeUtf8ToBase64("hello"))).toBe("hello");
    // 空串
    expect(client.decodeBase64ToUtf8(client.encodeUtf8ToBase64(""))).toBe("");

    mockInvoke.mockReset();
    mockInvoke.mockResolvedValue({ dataBase64: encoded });
    const back = await client.fsReadFileText("/vault/a.md");
    expect(back).toBe(text);
    expect(mockInvoke).toHaveBeenCalledWith("agent_request", {
      method: "fs/readFile",
      params: { path: "/vault/a.md" },
    });
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
