/**
 * AI 提示词模板 —— 选区快捷操作、对话上下文与链接/标签建议
 */
import type { ChatMessage } from "./ai-client";

/** 选区操作类型 */
export type SelectionAction = "continue" | "polish" | "summarize" | "expand";

/** 选区操作按钮配置 */
export const SELECTION_ACTIONS: { id: SelectionAction; label: string }[] = [
  { id: "continue", label: "续写" },
  { id: "polish", label: "润色" },
  { id: "summarize", label: "总结" },
  { id: "expand", label: "扩写" },
];

/** 截断到约 4000 字 */
function truncate(text: string, limit = 4000): string {
  if (text.length <= limit) return text;
  return text.slice(0, limit);
}

/** 选区操作对应的中文指令片段 */
function actionInstruction(action: SelectionAction): string {
  switch (action) {
    case "continue":
      return "请顺延选区的风格和内容继续续写，保持语言风格、语气一致，自然衔接，不要重复选区原文。";
    case "polish":
      return "请只改表达不改事实，对选区文本进行润色，提升流畅度与可读性，不要增删事实信息，返回改写后的全文。";
    case "summarize":
      return "请将选区内容总结为要点列表，简洁清晰，保留关键信息。";
    case "expand":
      return "请对选区内容进行扩写，补充细节、背景或例子，使内容更丰富、更具说服力。";
    default: {
      const _exhaustive: never = action;
      return String(_exhaustive);
    }
  }
}

/**
 * 构建选区快捷操作的 messages
 * - system 中包含笔记标题与动作要求，附带截断后的笔记上下文
 * - user 为选区文本
 * - noteBody 可选，截断约 4000 字作为上下文
 */
export function buildSelectionMessages(a: {
  action: SelectionAction;
  selection: string;
  noteTitle: string;
  noteBody?: string;
}): ChatMessage[] {
  const { action, selection, noteTitle, noteBody } = a;
  const instruction = actionInstruction(action);
  const ctx = noteBody ? `\n\n当前笔记内容（截断）：\n${truncate(noteBody)}` : "";
  const system: ChatMessage = {
    role: "system",
    content: `你是 wiki 笔记写作助手，围绕当前笔记《${noteTitle}》协助用户写作。${instruction}${ctx}`,
  };
  const user: ChatMessage = {
    role: "user",
    content: selection,
  };
  return [system, user];
}

/**
 * 构建通用对话 messages
 * - 有笔记上下文时前置一条 system，包含截断后的笔记内容
 * - 随后拼接历史消息
 */
export function buildChatMessages(a: {
  history: ChatMessage[];
  noteTitle?: string;
  noteBody?: string;
}): ChatMessage[] {
  const { history, noteTitle, noteBody } = a;
  if (noteTitle) {
    const ctx: ChatMessage = {
      role: "system",
      content: `以下为用户当前笔记《${noteTitle}》的内容：\n${truncate(noteBody ?? "")}`,
    };
    return [ctx, ...history];
  }
  return [...history];
}

/**
 * 构建链接/标签建议的 messages
 * - system 严格要求 JSON 输出，links 只能来自候选 key，tags 1-5 个短标签
 * - user 携带笔记标题、正文与候选列表
 */
export function buildLinkSuggestMessages(a: {
  noteTitle: string;
  noteBody: string;
  candidates: { key: string; title: string }[];
}): ChatMessage[] {
  const { noteTitle, noteBody, candidates } = a;
  const candidateKeys = candidates.map((c) => c.key).join(", ") || "（无）";
  const candidateList =
    candidates.length > 0 ? candidates.map((c) => `- ${c.key}: ${c.title}`).join("\n") : "（无候选）";
  const system: ChatMessage = {
    role: "system",
    content:
      `你是 wiki 笔记的链接与标签推荐助手。请根据用户提供的笔记内容，从候选列表中挑选最相关的笔记进行链接推荐，并生成 1-5 个简短的中文或英文标签。` +
      `严格输出 JSON：{"links":["key",...],"tags":["tag",...]}，其中 links 只能来自候选 key 列表 [${candidateKeys}]，tags 为 1-5 个简短中文/英文标签，不要输出任何额外文本或解释。`,
  };
  const user: ChatMessage = {
    role: "user",
    content: `笔记标题：《${noteTitle}》\n笔记内容：\n${truncate(noteBody)}\n候选笔记：\n${candidateList}`,
  };
  return [system, user];
}

/**
 * 解析链接建议的 JSON 输出
 * - 正则提取首个 {...} 块再 JSON.parse
 * - 失败返回 null
 * - links/tags 过滤非字符串项
 */
export function parseLinkSuggestJson(text: string): { links: string[]; tags: string[] } | null {
  const match = text.match(/\{[\s\S]*\}/);
  if (!match) return null;
  let obj: unknown;
  try {
    obj = JSON.parse(match[0]);
  } catch {
    return null;
  }
  if (obj == null || typeof obj !== "object" || Array.isArray(obj)) return null;
  const rec = obj as Record<string, unknown>;
  const rawLinks = rec["links"];
  const rawTags = rec["tags"];
  const links = Array.isArray(rawLinks) ? rawLinks.filter((v): v is string => typeof v === "string") : [];
  const tags = Array.isArray(rawTags) ? rawTags.filter((v): v is string => typeof v === "string") : [];
  return { links, tags };
}
