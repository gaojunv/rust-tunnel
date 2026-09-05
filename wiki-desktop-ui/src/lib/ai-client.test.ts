/**
 * ai-client / ai-prompts 单元测试
 * - fetch 用 vi.stubGlobal
 * - SSE 用手工 ReadableStream 分 chunk 推送（故意拆行，验证 buffer 逻辑）
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { AuthExpiredError, setToken, clearToken } from "./server-auth";
import { chatStream, chatOnce, listRelayModels } from "./ai-client";
import {
  parseLinkSuggestJson,
  buildSelectionMessages,
  buildChatMessages,
  buildLinkSuggestMessages,
  SELECTION_ACTIONS,
} from "./ai-prompts";

const BASE = "https://example.com";

// —— 辅助：构造 SSE 的 ReadableStream（按 chunk 推送） ——
function sseResponse(chunks: string[], status = 200): Response {
  const encoder = new TextEncoder();
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const c of chunks) controller.enqueue(encoder.encode(c));
      controller.close();
    },
  });
  return new Response(stream as unknown as BodyInit, {
    status,
    headers: { "content-type": "text/event-stream" },
  });
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function textResponse(text: string, status: number): Response {
  return new Response(text, { status });
}

describe("chatStream", () => {
  beforeEach(() => {
    setToken(BASE, "jwt-token");
    vi.stubGlobal("fetch", vi.fn());
  });
  afterEach(() => {
    clearToken(BASE);
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("正常流：多 delta 拼接顺序正确", async () => {
    const chunks = [
      `data: ${JSON.stringify({ choices: [{ delta: { content: "hello " } }] })}\n\n`,
      `data: ${JSON.stringify({ choices: [{ delta: { content: "world" } }] })}\n\n`,
      `data: [DONE]\n\n`,
    ];
    vi.mocked(fetch).mockResolvedValue(sseResponse(chunks));
    const out: string[] = [];
    for await (const d of chatStream({ baseUrl: BASE, model: "m", messages: [{ role: "user", content: "hi" }] })) {
      out.push(d);
    }
    expect(out).toEqual(["hello ", "world"]);
  });

  it("跨 chunk 行拼接", async () => {
    const payload = JSON.stringify({ choices: [{ delta: { content: "cross" } }] });
    const full = `data: ${payload}\n\n`;
    // 故意把一行拆到两个 chunk 边界
    const mid = Math.floor(full.length / 2);
    const c1 = full.slice(0, mid);
    const c2 = full.slice(mid) + `data: [DONE]\n\n`;
    vi.mocked(fetch).mockResolvedValue(sseResponse([c1, c2]));
    const out: string[] = [];
    for await (const d of chatStream({ baseUrl: BASE, model: "m", messages: [{ role: "user", content: "hi" }] })) {
      out.push(d);
    }
    expect(out).toEqual(["cross"]);
  });

  it("[DONE] 终止后续内容不产出", async () => {
    const chunks = [
      `data: ${JSON.stringify({ choices: [{ delta: { content: "a" } }] })}\n\n`,
      `data: [DONE]\n\n`,
      `data: ${JSON.stringify({ choices: [{ delta: { content: "should-not" } }] })}\n\n`,
    ];
    vi.mocked(fetch).mockResolvedValue(sseResponse(chunks));
    const out: string[] = [];
    for await (const d of chatStream({ baseUrl: BASE, model: "m", messages: [{ role: "user", content: "hi" }] })) {
      out.push(d);
    }
    expect(out).toEqual(["a"]);
  });

  it("坏 JSON 行跳过", async () => {
    const chunks = [
      `data: {bad json\n\n`,
      `data: ${JSON.stringify({ choices: [{ delta: { content: "ok" } }] })}\n\n`,
      `data: [DONE]\n\n`,
    ];
    vi.mocked(fetch).mockResolvedValue(sseResponse(chunks));
    const out: string[] = [];
    for await (const d of chatStream({ baseUrl: BASE, model: "m", messages: [{ role: "user", content: "hi" }] })) {
      out.push(d);
    }
    expect(out).toEqual(["ok"]);
  });

  it("delta 缺 content 跳过", async () => {
    const chunks = [
      `data: ${JSON.stringify({ choices: [{ delta: {} }] })}\n\n`,
      `data: ${JSON.stringify({ choices: [{ delta: { content: "" } }] })}\n\n`,
      `data: ${JSON.stringify({ choices: [{ delta: { content: "yes" } }] })}\n\n`,
      `data: [DONE]\n\n`,
    ];
    vi.mocked(fetch).mockResolvedValue(sseResponse(chunks));
    const out: string[] = [];
    for await (const d of chatStream({ baseUrl: BASE, model: "m", messages: [{ role: "user", content: "hi" }] })) {
      out.push(d);
    }
    expect(out).toEqual(["yes"]);
  });

  it("401 抛 AuthExpiredError", async () => {
    vi.mocked(fetch).mockResolvedValue(textResponse("unauthorized", 401));
    let err: unknown;
    try {
      const gen = chatStream({ baseUrl: BASE, model: "m", messages: [] });
      await gen.next();
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(AuthExpiredError);
  });

  it("无 token 抛 AuthExpiredError", async () => {
    clearToken(BASE);
    let err: unknown;
    try {
      const gen = chatStream({ baseUrl: BASE, model: "m", messages: [] });
      await gen.next();
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(AuthExpiredError);
  });

  it("非 2xx 抛 Error 且含状态码", async () => {
    vi.mocked(fetch).mockResolvedValue(textResponse("Internal Error " + "x".repeat(500), 500));
    let err: unknown;
    try {
      const gen = chatStream({ baseUrl: BASE, model: "m", messages: [] });
      await gen.next();
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(Error);
    expect((err as Error).message).toContain("500");
  });

  it("非 2xx 文本截断 300 字", async () => {
    const longText = "a".repeat(1000);
    vi.mocked(fetch).mockResolvedValue(textResponse(longText, 500));
    let err: unknown;
    try {
      const gen = chatStream({ baseUrl: BASE, model: "m", messages: [] });
      await gen.next();
    } catch (e) {
      err = e;
    }
    const msg = (err as Error).message;
    // 状态码 + 截断后的文本（不超过 300 字 + 前缀）
    expect(msg).toContain("500");
    // 截断部分不应包含完整 1000 个 a（最多 300）
    const afterColon = msg.split(":").slice(1).join(":");
    expect(afterColon.length).toBeLessThanOrEqual(310);
  });
});

describe("chatOnce", () => {
  beforeEach(() => {
    setToken(BASE, "jwt-token");
    vi.stubGlobal("fetch", vi.fn());
  });
  afterEach(() => {
    clearToken(BASE);
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("正常解析 choices[0].message.content", async () => {
    vi.mocked(fetch).mockResolvedValue(jsonResponse({ choices: [{ message: { content: "hello once" } }] }));
    const text = await chatOnce({ baseUrl: BASE, model: "m", messages: [{ role: "user", content: "hi" }] });
    expect(text).toBe("hello once");
  });

  it("缺 content 抛错", async () => {
    vi.mocked(fetch).mockResolvedValue(jsonResponse({ choices: [{ message: {} }] }));
    await expect(chatOnce({ baseUrl: BASE, model: "m", messages: [] })).rejects.toThrow();
  });

  it("401 抛 AuthExpiredError", async () => {
    vi.mocked(fetch).mockResolvedValue(textResponse("unauthorized", 401));
    await expect(chatOnce({ baseUrl: BASE, model: "m", messages: [] })).rejects.toBeInstanceOf(AuthExpiredError);
  });

  it("非 2xx 抛 Error 含状态码", async () => {
    vi.mocked(fetch).mockResolvedValue(textResponse("bad", 422));
    await expect(chatOnce({ baseUrl: BASE, model: "m", messages: [] })).rejects.toThrow(/422/);
  });
});

describe("listRelayModels", () => {
  beforeEach(() => {
    setToken(BASE, "jwt-token");
    vi.stubGlobal("fetch", vi.fn());
  });
  afterEach(() => {
    clearToken(BASE);
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("正常解析 data[].id", async () => {
    vi.mocked(fetch).mockResolvedValue(jsonResponse({ object: "list", data: [{ id: "gpt-4" }, { id: "gpt-3.5" }] }));
    const ids = await listRelayModels(BASE);
    expect(ids).toEqual(["gpt-4", "gpt-3.5"]);
  });

  it("宽容解析：非字符串 id 过滤", async () => {
    vi.mocked(fetch).mockResolvedValue(
      jsonResponse({ object: "list", data: [{ id: "a" }, { id: 123 }, {}, { id: null }] }),
    );
    const ids = await listRelayModels(BASE);
    expect(ids).toEqual(["a"]);
  });

  it("401 抛 AuthExpiredError", async () => {
    vi.mocked(fetch).mockResolvedValue(textResponse("unauthorized", 401));
    await expect(listRelayModels(BASE)).rejects.toBeInstanceOf(AuthExpiredError);
  });
});

describe("SELECTION_ACTIONS", () => {
  it("包含 4 种操作", () => {
    expect(SELECTION_ACTIONS.map((a) => a.id)).toEqual(["continue", "polish", "summarize", "expand"]);
  });
});

describe("buildSelectionMessages", () => {
  it("system 含标题与动作要求，user 为选区", () => {
    const msgs = buildSelectionMessages({
      action: "polish",
      selection: "选区文本",
      noteTitle: "我的笔记",
      noteBody: "正文",
    });
    expect(msgs).toHaveLength(2);
    expect(msgs[0].role).toBe("system");
    expect(msgs[0].content).toContain("我的笔记");
    expect(msgs[1].role).toBe("user");
    expect(msgs[1].content).toBe("选区文本");
  });

  it("noteBody 可选：无则不含上下文段", () => {
    const msgs = buildSelectionMessages({
      action: "continue",
      selection: "sel",
      noteTitle: "t",
    });
    expect(msgs).toHaveLength(2);
    expect(msgs[0].content).toContain("t");
  });
});

describe("buildChatMessages", () => {
  it("有笔记上下文前置 system", () => {
    const msgs = buildChatMessages({
      history: [{ role: "user", content: "hi" }],
      noteTitle: "标题",
      noteBody: "正文内容",
    });
    expect(msgs[0].role).toBe("system");
    expect(msgs[0].content).toContain("标题");
    expect(msgs[1].content).toBe("hi");
  });

  it("无笔记上下文直接返回 history", () => {
    const h: { role: "user"; content: string }[] = [{ role: "user", content: "hi" }];
    const msgs = buildChatMessages({ history: h });
    expect(msgs).toEqual(h);
  });
});

describe("buildLinkSuggestMessages", () => {
  it("system 要求 JSON 输出且含候选 key", () => {
    const msgs = buildLinkSuggestMessages({
      noteTitle: "t",
      noteBody: "body",
      candidates: [
        { key: "a/b", title: "A" },
        { key: "c/d", title: "C" },
      ],
    });
    expect(msgs[0].content).toContain("a/b");
    expect(msgs[0].content).toContain("c/d");
    expect(msgs[0].content).toContain("links");
    expect(msgs[1].content).toContain("t");
  });
});

describe("parseLinkSuggestJson", () => {
  it("正常 JSON", () => {
    const r = parseLinkSuggestJson('{"links":["a/b"],"tags":["t1","t2"]}');
    expect(r).toEqual({ links: ["a/b"], tags: ["t1", "t2"] });
  });

  it("带前后废话的 JSON 块", () => {
    const r = parseLinkSuggestJson('前言废话 {"links":["a"],"tags":["x"]} 后面还有废话');
    expect(r).toEqual({ links: ["a"], tags: ["x"] });
  });

  it("非法返回 null", () => {
    expect(parseLinkSuggestJson("not json at all")).toBeNull();
    expect(parseLinkSuggestJson("{bad json}")).toBeNull();
  });

  it("非字符串项过滤", () => {
    const r = parseLinkSuggestJson('{"links":["a",123,null],"tags":[456,"ok"]}');
    expect(r).toEqual({ links: ["a"], tags: ["ok"] });
  });

  it("缺字段容错为空数组", () => {
    const r = parseLinkSuggestJson('{"links":["a"]}');
    expect(r).toEqual({ links: ["a"], tags: [] });
  });
});
