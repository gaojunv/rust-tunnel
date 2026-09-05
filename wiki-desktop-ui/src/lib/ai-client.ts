/**
 * LLM 中转客户端 —— 封装服务端 OpenAI 兼容 relay 接口
 * 依赖 `server-auth.ts` 的 JWT 存储与过期错误
 */
import { AuthExpiredError, clearToken, getToken } from "./server-auth";

/** 聊天消息 */
export interface ChatMessage {
  role: "system" | "user" | "assistant";
  content: string;
}

/** 去尾随 `/` 的本地归一化（与 server-auth.normalizeBaseUrl 一致） */
function normalizeBaseUrl(baseUrl: string): string {
  return baseUrl.trim().replace(/\/+$/, "");
}

/** 读取错误响应文本并截断 300 字 */
async function readErrorText(resp: Response): Promise<string> {
  try {
    const t = await resp.text();
    return t.slice(0, 300);
  } catch {
    return "";
  }
}

/**
 * 流式聊天：逐段 yield assistant delta 文本
 * - 无 token 抛 AuthExpiredError
 * - 401 清理 token 并抛 AuthExpiredError
 * - 其它非 2xx 读文本截断 300 字抛 Error
 * - SSE 通过 reader + TextDecoder 增量解码，buffer 到 \n 边界
 */
export async function* chatStream(opts: {
  baseUrl: string;
  model: string;
  messages: ChatMessage[];
  signal?: AbortSignal;
}): AsyncGenerator<string, void, unknown> {
  const { baseUrl, model, messages, signal } = opts;
  const norm = normalizeBaseUrl(baseUrl);
  const token = getToken(baseUrl);
  if (!token) throw new AuthExpiredError();

  const resp = await fetch(`${norm}/api/llm/relay/chat/completions`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${token}`,
    },
    body: JSON.stringify({ model, messages, stream: true }),
    signal,
  });

  if (resp.status === 401) {
    clearToken(baseUrl);
    throw new AuthExpiredError();
  }
  if (!resp.ok) {
    const txt = await readErrorText(resp);
    throw new Error(`LLM relay ${resp.status}: ${txt}`);
  }

  if (!resp.body) return;

  const reader = resp.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  // 中止时取消 reader
  let abortHandler: (() => void) | null = null;
  if (signal) {
    abortHandler = () => {
      reader.cancel().catch(() => {});
    };
    if (signal.aborted) {
      abortHandler();
    } else {
      signal.addEventListener("abort", abortHandler, { once: true });
    }
  }

  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });

      let idx: number;
      while ((idx = buffer.indexOf("\n")) !== -1) {
        let line = buffer.slice(0, idx);
        buffer = buffer.slice(idx + 1);
        if (line.endsWith("\r")) line = line.slice(0, -1);
        if (!line.startsWith("data: ")) continue;
        const data = line.slice(6);
        if (data === "[DONE]") return;
        if (!data) continue;
        try {
          const json = JSON.parse(data);
          const content: unknown = json?.choices?.[0]?.delta?.content;
          if (typeof content === "string" && content.length > 0) {
            yield content;
          }
        } catch {
          // 解析失败跳过
        }
      }
    }

    // flush 解码器残余
    buffer += decoder.decode();
    if (buffer) {
      const parts = buffer.split("\n");
      for (let line of parts) {
        if (line.endsWith("\r")) line = line.slice(0, -1);
        if (!line.startsWith("data: ")) continue;
        const data = line.slice(6);
        if (data === "[DONE]") return;
        if (!data) continue;
        try {
          const json = JSON.parse(data);
          const content: unknown = json?.choices?.[0]?.delta?.content;
          if (typeof content === "string" && content.length > 0) {
            yield content;
          }
        } catch {
          // 跳过
        }
      }
    }
  } finally {
    if (signal && abortHandler) {
      try {
        signal.removeEventListener("abort", abortHandler);
      } catch {
        // 忽略
      }
    }
    if (signal?.aborted) {
      try {
        await reader.cancel();
      } catch {
        // 忽略
      }
    }
  }
}

/**
 * 非流式聊天：一次性返回完整文本
 */
export async function chatOnce(opts: {
  baseUrl: string;
  model: string;
  messages: ChatMessage[];
  signal?: AbortSignal;
}): Promise<string> {
  const { baseUrl, model, messages, signal } = opts;
  const norm = normalizeBaseUrl(baseUrl);
  const token = getToken(baseUrl);
  if (!token) throw new AuthExpiredError();

  const resp = await fetch(`${norm}/api/llm/relay/chat/completions`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${token}`,
    },
    body: JSON.stringify({ model, messages, stream: false }),
    signal,
  });

  if (resp.status === 401) {
    clearToken(baseUrl);
    throw new AuthExpiredError();
  }
  if (!resp.ok) {
    const txt = await readErrorText(resp);
    throw new Error(`LLM relay ${resp.status}: ${txt}`);
  }

  let json: unknown;
  try {
    json = await resp.json();
  } catch {
    throw new Error("LLM relay 返回非 JSON");
  }
  const content: unknown = (json as { choices?: { message?: { content?: unknown } }[] })?.choices?.[0]?.message?.content;
  if (typeof content !== "string") {
    throw new Error("LLM relay 响应缺失 content");
  }
  return content;
}

/**
 * 列出可用模型 id 列表
 */
export async function listRelayModels(baseUrl: string, signal?: AbortSignal): Promise<string[]> {
  const norm = normalizeBaseUrl(baseUrl);
  const token = getToken(baseUrl);
  if (!token) throw new AuthExpiredError();

  const resp = await fetch(`${norm}/api/llm/relay/models`, {
    method: "GET",
    headers: {
      Authorization: `Bearer ${token}`,
    },
    signal,
  });

  if (resp.status === 401) {
    clearToken(baseUrl);
    throw new AuthExpiredError();
  }
  if (!resp.ok) {
    const txt = await readErrorText(resp);
    throw new Error(`LLM relay ${resp.status}: ${txt}`);
  }

  let json: unknown;
  try {
    json = await resp.json();
  } catch {
    return [];
  }
  const data: unknown = (json as { data?: unknown })?.data;
  if (!Array.isArray(data)) return [];
  return data
    .map((item: unknown) => (item as { id?: unknown })?.id)
    .filter((id: unknown): id is string => typeof id === "string");
}
