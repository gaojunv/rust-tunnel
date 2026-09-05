/**
 * 服务端 HTTP 层 —— 知识容器 Wiki 页面同步
 * 注入 JWT、分页拉取、ref 编码、配置持久化
 */

import { AuthExpiredError, clearToken, getToken, setToken } from "../lib/server-auth";

/** 知识容器信息（宽容解析后） */
export interface KnowledgeSourceInfo {
  id: string;
  name: string;
  kind: string;
  docCount: number;
  pageCount: number;
}

/** 远端页面摘要 */
export interface RemotePageSummary {
  ref: string;
  title: string;
  locked: boolean;
  updated_at: string;
}

/** 远端完整页面 */
export interface RemotePage extends RemotePageSummary {
  summary: string;
  content: string;
}

/** 服务端返回的非 2xx 错误 */
export class ServerError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "ServerError";
    this.status = status;
  }
}

/** 同步配置（密码不持久化） */
export interface SyncConfig {
  baseUrl: string;
  knowledgeId: string;
  propagateDeletes: boolean;
}

const SYNC_CONFIG_KEY = "wiki.sync.config.v1";

/** 规整 baseUrl：去尾随 `/` 并 trim */
function normalizeBaseUrl(baseUrl: string): string {
  return baseUrl.trim().replace(/\/+$/, "");
}

/**
 * 将 ref 按段编码，避免 `/` 被转义而 `*ref` 通配失效
 */
function encodeRef(ref: string): string {
  return ref
    .split("/")
    .map((seg) => encodeURIComponent(seg))
    .join("/");
}

/**
 * 内部请求：注入 Authorization，处理 401 与通用错误
 */
async function fetchJson<T>(baseUrl: string, path: string, init?: RequestInit): Promise<T> {
  const base = normalizeBaseUrl(baseUrl);
  const url = `${base}${path}`;
  const token = getToken(baseUrl);
  const headers: Record<string, string> = {
    "Content-Type": "application/json",
    ...(init?.headers as Record<string, string> | undefined),
  };
  if (token) headers["Authorization"] = `Bearer ${token}`;

  const resp = await fetch(url, { ...init, headers });
  if (resp.status === 401) {
    clearToken(baseUrl);
    throw new AuthExpiredError("认证已过期");
  }
  if (!resp.ok) {
    const text = await resp.text().catch(() => "");
    const truncated = text.slice(0, 200);
    throw new ServerError(resp.status, truncated || `HTTP ${resp.status}`);
  }
  // 204 无内容
  if (resp.status === 204) return undefined as unknown as T;
  const data = await resp.json().catch(() => {
    throw new ServerError(resp.status, "响应非 JSON");
  });
  return data as T;
}

// —— 登录与容器列表 ——

/**
 * 登录并持久化 token
 * @param baseUrl 服务端地址
 * @param password 密码
 */
export async function login(
  baseUrl: string,
  password: string,
): Promise<{ token: string; authRequired: boolean }> {
  const base = normalizeBaseUrl(baseUrl);
  const resp = await fetch(`${base}/api/login`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ password }),
  });
  if (resp.status === 401) {
    const text = await resp.text().catch(() => "");
    throw new ServerError(401, text.slice(0, 200) || "密码错误");
  }
  if (!resp.ok) {
    const text = await resp.text().catch(() => "");
    throw new ServerError(resp.status, text.slice(0, 200) || `HTTP ${resp.status}`);
  }
  const data = (await resp.json().catch(() => ({}))) as Record<string, unknown>;
  const token = String(data["token"] ?? data["access_token"] ?? "");
  const authRequired =
    (data["auth_required"] as boolean | undefined) ??
    (data["authRequired"] as boolean | undefined) ??
    true;
  if (!token) throw new ServerError(500, "登录响应缺少 token");
  setToken(baseUrl, token);
  return { token, authRequired: Boolean(authRequired) };
}

/**
 * 列出知识容器（宽容解析）
 */
export async function listKnowledgeSources(baseUrl: string): Promise<KnowledgeSourceInfo[]> {
  const raw = await fetchJson<{ sources?: unknown[]; data?: unknown[] }>(
    baseUrl,
    "/api/knowledge",
  );
  const arr = (raw?.sources ?? raw?.data ?? []) as unknown[];
  if (!Array.isArray(arr)) return [];
  return arr.map(parseSource);
}

/** 宽容解析单个容器 */
function parseSource(raw: unknown): KnowledgeSourceInfo {
  const r = (raw ?? {}) as Record<string, unknown>;
  return {
    id: String(r["id"] ?? r["ID"] ?? ""),
    name: String(r["name"] ?? ""),
    kind: String(r["kind"] ?? r["scope_type"] ?? r["index_kind"] ?? ""),
    docCount: toNumber(r["doc_count"] ?? r["docCount"] ?? r["doc_count"] ?? 0),
    pageCount: toNumber(
      r["page_count"] ?? r["pageCount"] ?? r["page_count"] ?? r["pageCount"] ?? 0,
    ),
  };
}

function toNumber(v: unknown): number {
  const n = Number(v);
  return Number.isFinite(n) ? n : 0;
}

// —— 页面 API ——

export interface ServerApi {
  listAllPages(): Promise<RemotePageSummary[]>;
  getPage(ref: string): Promise<RemotePage | null>;
  putPage(ref: string, body: { title: string; summary: string; content: string }): Promise<RemotePage>;
  deletePage(ref: string): Promise<boolean>;
}

/** 解析摘要（宽容） */
function parsePageSummary(raw: unknown): RemotePageSummary {
  const r = (raw ?? {}) as Record<string, unknown>;
  return {
    ref: String(r["ref"] ?? r["page_ref"] ?? ""),
    title: String(r["title"] ?? ""),
    locked: Boolean(r["locked"]),
    updated_at: String(r["updated_at"] ?? r["updatedAt"] ?? r["updated_at"] ?? ""),
  };
}

/** 解析完整页 */
function parsePage(raw: unknown): RemotePage {
  const r = (raw ?? {}) as Record<string, unknown>;
  return {
    ref: String(r["ref"] ?? r["page_ref"] ?? ""),
    title: String(r["title"] ?? ""),
    locked: Boolean(r["locked"]),
    updated_at: String(r["updated_at"] ?? r["updatedAt"] ?? ""),
    summary: String(r["summary"] ?? ""),
    content: String(r["content"] ?? ""),
  };
}

/**
 * 创建绑定到指定容器的服务端 API
 */
export function createServerApi(baseUrl: string, knowledgeId: string): ServerApi {
  const encodedId = encodeURIComponent(knowledgeId);

  return {
    async listAllPages(): Promise<RemotePageSummary[]> {
      const out: RemotePageSummary[] = [];
      const limit = 200;
      // 用 for 循环替代 while(true) 以通过 no-constant-condition
      for (let offset = 0; offset <= 100_000; offset += limit) {
        const data = await fetchJson<{ pages?: unknown[]; total?: number }>(
          baseUrl,
          `/api/knowledge/${encodedId}/pages?limit=${limit}&offset=${offset}`,
        );
        const pages = Array.isArray(data?.pages) ? data.pages : [];
        for (const p of pages) out.push(parsePageSummary(p));
        if (pages.length < limit) break;
      }
      return out;
    },

    async getPage(ref: string): Promise<RemotePage | null> {
      const enc = encodeRef(ref);
      try {
        const data = await fetchJson<unknown>(baseUrl, `/api/knowledge/${encodedId}/pages/${enc}`);
        return parsePage(data);
      } catch (e) {
        if (e instanceof ServerError && e.status === 404) return null;
        throw e;
      }
    },

    async putPage(
      ref: string,
      body: { title: string; summary: string; content: string },
    ): Promise<RemotePage> {
      const enc = encodeRef(ref);
      const data = await fetchJson<unknown>(baseUrl, `/api/knowledge/${encodedId}/pages/${enc}`, {
        method: "PUT",
        body: JSON.stringify(body),
      });
      return parsePage(data);
    },

    async deletePage(ref: string): Promise<boolean> {
      const enc = encodeRef(ref);
      try {
        await fetchJson<unknown>(baseUrl, `/api/knowledge/${encodedId}/pages/${enc}`, {
          method: "DELETE",
        });
        return true;
      } catch (e) {
        if (e instanceof ServerError && e.status === 404) return false;
        throw e;
      }
    },
  };
}

// —— 同步配置持久化 ——

/**
 * 加载同步配置
 */
export function loadSyncConfig(): SyncConfig | null {
  try {
    const raw = localStorage.getItem(SYNC_CONFIG_KEY);
    if (!raw) return null;
    const obj = JSON.parse(raw) as Record<string, unknown>;
    if (typeof obj["baseUrl"] !== "string" || typeof obj["knowledgeId"] !== "string") return null;
    return {
      baseUrl: String(obj["baseUrl"]),
      knowledgeId: String(obj["knowledgeId"]),
      propagateDeletes: Boolean(obj["propagateDeletes"]),
    };
  } catch {
    return null;
  }
}

/**
 * 保存同步配置（不含密码）
 */
export function saveSyncConfig(cfg: SyncConfig): void {
  try {
    localStorage.setItem(SYNC_CONFIG_KEY, JSON.stringify(cfg));
  } catch {
    // 隐私模式忽略
  }
}
