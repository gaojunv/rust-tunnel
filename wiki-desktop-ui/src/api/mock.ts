import type { DeleteFolderResult, NoteDto, NoteSummary, RenameFolderResult, SearchHitDto, GraphDto, VaultInfo } from "./types";
import type { KnowledgeSourceInfo, RemotePage } from "./server";

// —— 工具：从 body 中提取 [[wikilink]] ——
function extractLinks(body: string): string[] {
  const re = /\[\[([^\]|]+)(?:\|[^\]]+)?\]\]/g;
  const out: string[] = [];
  let m: RegExpExecArray | null;
  while ((m = re.exec(body))) out.push(m[1].trim());
  return out;
}

function nowSec(): number {
  return Math.floor(Date.now() / 1000);
}

// 格式化为 "YYYY-MM-DD HH:MM:SS"（UTC，与 parseRemoteTime 对齐）
function formatUtcNow(): string {
  const d = new Date();
  const pad = (n: number, len = 2) => String(n).padStart(len, "0");
  return `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())} ${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}:${pad(d.getUTCSeconds())}`;
}

// 初始 5 篇互相引用的中文笔记：覆盖有 tags / 有别名 / 有断链 / 一个孤儿
const SEED: NoteDto[] = [
  {
    key: "index",
    title: "首页",
    aliases: ["主页"],
    tags: ["导航"],
    body: [
      "# 欢迎使用 Wiki Desktop",
      "",
      "这是本地离线 Wiki 的首页。快速导航：",
      "- [[rust-tunnel 概览]] — 项目总览与架构",
      "- [[待办清单]] — 演示断链：其中有一条指向不存在的笔记",
      "- [[使用指南]] — 编辑、搜索与图谱用法",
      "",
      "> 小技巧：在正文中用 [[笔记标题或 key]] 创建双链。",
    ].join("\n"),
    modified: nowSec() - 3600 * 5,
  },
  {
    key: "rust-tunnel-overview",
    title: "rust-tunnel 概览",
    aliases: [],
    tags: ["rust-tunnel", "架构"],
    body: [
      "# rust-tunnel 概览",
      "",
      "rust-tunnel 是基于 Rust 的内网穿透工具 + 前端管理台。",
      "",
      "## 相关笔记",
      "- [[使用指南]]",
      "- [[首页|index]]",
      "",
      "## 架构要点",
      "- 控制通道：TLS + bincode",
      "- 反向代理：直连 / 隧道 两类 backend",
    ].join("\n"),
    modified: nowSec() - 3600 * 2,
  },
  {
    key: "usage-guide",
    title: "使用指南",
    aliases: ["guide"],
    tags: ["指南"],
    body: [
      "# 使用指南",
      "",
      "## 编辑",
      "在中间编辑器修改标题与正文，点击“保存”即可。",
      "",
      "## 搜索",
      "左侧搜索框输入关键词，实时调用 `search_notes`。",
      "",
      "## 图谱",
      "右侧面板基于 `get_graph` 的出链/反链推导占位展示，后续会替换为力导向图。",
      "",
      "返回 [[首页]] 或查看 [[rust-tunnel 概览]]。",
    ].join("\n"),
    modified: nowSec() - 900,
  },
  {
    key: "todo-list",
    title: "待办清单",
    aliases: [],
    tags: ["待办"],
    body: [
      "# 待办清单",
      "",
      "- [x] 搭建 wiki-desktop-ui 骨架",
      "- [ ] 接入真实 Tauri 后端",
      "- [ ] 力导向图谱可视化",
      "- [ ] 关联一条断链用于演示：[[不存在的笔记]]",
      "",
      "回到 [[首页]]。",
    ].join("\n"),
    modified: nowSec() - 600,
  },
  {
    key: "orphan-note",
    title: "孤儿笔记",
    aliases: [],
    tags: [],
    body: [
      "# 孤儿笔记",
      "",
      "这篇笔记没有被任何笔记引用，也没有引用他人——用于演示孤儿节点。",
      "",
      "它只有一个标签都没有的安静角落。",
    ].join("\n"),
    modified: nowSec() - 120,
  },
];

// 可变内存库（save/delete 会改这里）
const store = new Map<string, NoteDto>(SEED.map((n) => [n.key, { ...n }]));

// 同步状态内存存储（对应 read_sync_state / write_sync_state）
let syncStateJson: string | null = null;

function toSummary(n: NoteDto): NoteSummary {
  return { key: n.key, title: n.title, tags: [...n.tags], modified: n.modified };
}

function buildGraph(): GraphDto {
  const nodes = [...store.values()].map((n) => ({ key: n.key, title: n.title }));
  const keys = new Set(store.keys());
  const edges: GraphDto["edges"] = [];
  for (const n of store.values()) {
    for (const target of extractLinks(n.body)) {
      // 仅对已存在的目标建边，断链不在图中体现（由 GraphPanel 单独计算）
      if (keys.has(target)) edges.push({ from: n.key, to: target });
    }
  }
  return { nodes, edges };
}

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function snippetAround(body: string, query: string, len = 80): string {
  const idx = body.toLowerCase().indexOf(query.toLowerCase());
  if (idx === -1) return body.slice(0, len);
  const start = Math.max(0, idx - 32);
  return (start > 0 ? "…" : "") + body.slice(start, start + len) + (start + len < body.length ? "…" : "");
}

// 与 Rust 侧同名的 mock 命令分发
export const mockVault = {
  getVaultInfo(): VaultInfo {
    return { root: "/mock/vault", note_count: store.size };
  },

  listNotes(): NoteSummary[] {
    return [...store.values()]
      .sort((a, b) => b.modified - a.modified)
      .map(toSummary);
  },

  listNotesFull(): NoteDto[] {
    return [...store.values()]
      .sort((a, b) => b.modified - a.modified)
      .map((n) => ({ ...n, aliases: [...n.aliases], tags: [...n.tags] }));
  },

  readSyncState(): string | null {
    return syncStateJson;
  },

  writeSyncState(json: string): void {
    syncStateJson = json;
  },

  // 与 Rust 侧一致：笔记不存在时抛错（IpcError::NoteNotFound），而非返回 null
  getNote(key: string): NoteDto {
    const n = store.get(key);
    if (!n) throw new Error(`笔记不存在: ${key}`);
    return { ...n, aliases: [...n.aliases], tags: [...n.tags] };
  },

  saveNote(key: string, body: string, title?: string): NoteDto {
    const existing = store.get(key);
    const next: NoteDto = {
      key,
      title: title?.trim() || existing?.title || key,
      aliases: existing ? [...existing.aliases] : [],
      tags: existing ? [...existing.tags] : [],
      body,
      modified: nowSec(),
    };
    store.set(key, next);
    return { ...next };
  },

  // 与 Rust 侧一致：返回 ()，不存在时抛错
  deleteNote(key: string): void {
    if (!store.delete(key)) throw new Error(`笔记不存在: ${key}`);
  },

  // 与 Rust 侧一致：limit 为必填 usize
  searchNotes(query: string, limit: number): SearchHitDto[] {
    const q = query.trim().toLowerCase();
    if (!q) return [];
    const hits: SearchHitDto[] = [];
    for (const n of store.values()) {
      const hay = `${n.title}\n${n.body}`.toLowerCase();
      if (!hay.includes(q)) continue;
      // 简单计分：标题命中更高
      const score = n.title.toLowerCase().includes(q) ? 2 : 1;
      hits.push({
        note_key: n.key,
        title: n.title,
        snippet: snippetAround(n.body, q),
        score,
      });
    }
    hits.sort((a, b) => b.score - a.score);
    return hits.slice(0, limit);
  },

  renameNote(key: string, newKey: string, rewriteLinks: boolean): NoteDto {
    const src = store.get(key);
    if (!src) throw new Error(`笔记不存在: ${key}`);
    if (store.has(newKey)) throw new Error("已存在同名笔记");
    const next: NoteDto = { ...src, key: newKey, title: newKey, modified: nowSec() };
    store.delete(key);
    store.set(newKey, next);
    if (rewriteLinks) {
      const pattern = new RegExp(`\\[\\[${escapeRegExp(key)}(?=[\\]|\\])`, "g");
      let rewrittenCount = 0;
      for (const n of store.values()) {
        if (n.body.includes(`[[${key}`)) {
          const before = n.body;
          n.body = n.body.replace(pattern, `[[${newKey}`);
          if (n.body !== before) rewrittenCount++;
        }
      }
      void rewrittenCount;
    }
    return { ...next, aliases: [...next.aliases], tags: [...next.tags] };
  },

  renameFolder(oldPrefix: string, newPrefix: string, rewriteLinks: boolean): RenameFolderResult {
    const moved: RenameFolderResult["moved"] = [];
    const failed: RenameFolderResult["failed"] = [];
    const toMove: Array<[string, string]> = [];
    for (const k of store.keys()) {
      if (k === oldPrefix || k.startsWith(oldPrefix + "/")) {
        const suffix = k.slice(oldPrefix.length);
        const nk = newPrefix + suffix;
        toMove.push([k, nk]);
      }
    }
    // 冲突检测：目标已存在且不在本次移动集合中（或重复目标）
    const movingFromSet = new Set(toMove.map(([f]) => f));
    const targetSet = new Set<string>();
    for (const [from, to] of toMove) {
      if (targetSet.has(to)) {
        failed.push({ key: from, error: "目标已存在" });
        continue;
      }
      if (store.has(to) && !movingFromSet.has(to)) {
        failed.push({ key: from, error: "目标已存在" });
        continue;
      }
      targetSet.add(to);
    }
    const failedFrom = new Set(failed.map((f) => f.key));
    const succeeded = toMove.filter(([f]) => !failedFrom.has(f));
    // 执行移动
    for (const [from, to] of succeeded) {
      const src = store.get(from)!;
      const next: NoteDto = { ...src, key: to, title: to, modified: nowSec() };
      store.delete(from);
      store.set(to, next);
      moved.push({ from_key: from, to_key: to });
    }
    // 链接重写：简单字符串替换 [[oldPrefix
    let link_rewritten: string[] = [];
    let rewritten_count = 0;
    if (rewriteLinks && succeeded.length > 0) {
      const pattern = new RegExp(`\\[\\[${escapeRegExp(oldPrefix)}(?=[\\]/|\\])`, "g");
      for (const n of store.values()) {
        if (n.body.includes(`[[${oldPrefix}`)) {
          const before = n.body;
          n.body = n.body.replace(pattern, `[[${newPrefix}`);
          if (n.body !== before) {
            link_rewritten.push(n.key);
            rewritten_count++;
          }
        }
      }
    }
    return { moved, failed, link_rewritten, rewritten_count };
  },

  deleteFolder(prefix: string): DeleteFolderResult {
    const toDelete: string[] = [];
    for (const k of store.keys()) {
      if (k === prefix || k.startsWith(prefix + "/")) toDelete.push(k);
    }
    const deleted: string[] = [];
    const failed: DeleteFolderResult["failed"] = [];
    for (const k of toDelete) {
      if (store.delete(k)) deleted.push(k);
      else failed.push({ key: k, error: "删除失败" });
    }
    return { deleted, failed };
  },

  getGraph(): GraphDto {
    return buildGraph();
  },
};

export type MockCommand =
  | "get_vault_info"
  | "list_notes"
  | "get_note"
  | "save_note"
  | "delete_note"
  | "search_notes"
  | "get_graph"
  | "rename_note"
  | "rename_folder"
  | "delete_folder"
  | "list_notes_full"
  | "read_sync_state"
  | "write_sync_state";

// —— 内存假服务器（仅用于非 Tauri 环境的 fetch 拦截） ——

export const MOCK_BASE_URL = "mock://local";
const MOCK_TOKEN = "mock-token";
const MOCK_KB_ID = "mock-wiki";

// 远端页面存储
const mockRemotePages = new Map<string, RemotePage>();

// 辅助：构造 JSON Response
function jsonResponse(data: unknown, status = 200): Response {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

// 处理 mock 请求
async function handleMockRequest(urlStr: string, init?: RequestInit): Promise<Response> {
  const url = new URL(urlStr);
  const method = (init?.method ?? "GET").toUpperCase();
  const pathname = url.pathname;

  // 登录
  if (pathname === "/api/login" && method === "POST") {
    const bodyText = typeof init?.body === "string" ? (init.body as string) : "";
    let pwd = "";
    try {
      const j = JSON.parse(bodyText) as Record<string, unknown>;
      pwd = String(j["password"] ?? "");
    } catch {
      pwd = "";
    }
    // 密码为 "mock" 或空串时成功（兼容 auth_required:false 场景）
    if (pwd === "mock" || pwd === "") {
      return jsonResponse({ token: MOCK_TOKEN, auth_required: false });
    }
    return new Response("密码错误", { status: 401 });
  }

  // 容器列表
  if (pathname === "/api/knowledge" && method === "GET") {
    const source: KnowledgeSourceInfo = {
      id: MOCK_KB_ID,
      name: "演示知识库",
      kind: "wiki",
      docCount: 0,
      pageCount: mockRemotePages.size,
    };
    return jsonResponse({ sources: [source] });
  }

  // 页面分页
  const pagesListRe = /^\/api\/knowledge\/([^/]+)\/pages$/;
  const mList = pathname.match(pagesListRe);
  if (mList && method === "GET") {
    const kid = decodeURIComponent(mList[1]);
    if (kid !== MOCK_KB_ID) return jsonResponse({ pages: [] });
    const limit = Math.min(Number(url.searchParams.get("limit") ?? "200"), 200);
    const offset = Number(url.searchParams.get("offset") ?? "0");
    const all = [...mockRemotePages.values()].sort((a, b) => a.ref.localeCompare(b.ref));
    const slice = all.slice(offset, offset + limit).map((p) => ({
      ref: p.ref,
      title: p.title,
      locked: p.locked,
      updated_at: p.updated_at,
    }));
    return jsonResponse({ pages: slice, total: all.length });
  }

  // 单页 CRUD：/api/knowledge/:id/pages/*ref
  const pageRe = /^\/api\/knowledge\/([^/]+)\/pages\/(.+)$/;
  const mPage = pathname.match(pageRe);
  if (mPage) {
    const kid = decodeURIComponent(mPage[1]);
    if (kid !== MOCK_KB_ID) return new Response("not found", { status: 404 });
    const ref = mPage[2].split("/").map(decodeURIComponent).join("/");
    if (method === "GET") {
      const page = mockRemotePages.get(ref);
      if (!page) return new Response("not found", { status: 404 });
      return jsonResponse(page);
    }
    if (method === "PUT") {
      const bodyText = typeof init?.body === "string" ? (init.body as string) : "";
      let parsed: Record<string, unknown> = {};
      try {
        parsed = JSON.parse(bodyText) as Record<string, unknown>;
      } catch {
        parsed = {};
      }
      const title = String(parsed["title"] ?? ref);
      const content = String(parsed["content"] ?? "");
      const summary = String(parsed["summary"] ?? "");
      const page: RemotePage = {
        ref,
        title,
        summary,
        content,
        locked: false,
        updated_at: formatUtcNow(),
      };
      mockRemotePages.set(ref, page);
      return jsonResponse(page);
    }
    if (method === "DELETE") {
      const existed = mockRemotePages.delete(ref);
      if (!existed) return new Response("not found", { status: 404 });
      return new Response(null, { status: 204 });
    }
  }

  return new Response("not found", { status: 404 });
}

// 内存假服务器句柄（供测试或直接调用）
export const mockServer = {
  get pages() {
    return mockRemotePages;
  },
  clear() {
    mockRemotePages.clear();
  },
  login(password: string): { token: string } {
    if (password === "mock" || password === "") return { token: MOCK_TOKEN };
    throw new Error("密码错误");
  },
  listKnowledgeSources(): KnowledgeSourceInfo[] {
    return [{ id: MOCK_KB_ID, name: "演示知识库", kind: "wiki", docCount: 0, pageCount: mockRemotePages.size }];
  },
};

let interceptorInstalled = false;

// 安装 fetch 拦截器：非 Tauri 且 baseUrl 为 mock 默认值时路由到内存假服务器
export function installMockServerInterceptor(): void {
  if (interceptorInstalled) return;
  if (typeof window === "undefined") return;
  interceptorInstalled = true;
  const origFetch = window.fetch.bind(window);
  window.fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const urlStr =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.toString()
          : (input as Request).url;
    if (!urlStr.startsWith(MOCK_BASE_URL)) {
      return origFetch(input as RequestInfo, init);
    }
    return handleMockRequest(urlStr, init);
  };
}
