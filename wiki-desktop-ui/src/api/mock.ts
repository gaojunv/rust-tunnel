import type { NoteDto, NoteSummary, SearchHitDto, GraphDto, VaultInfo } from "./types";

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
  | "get_graph";
