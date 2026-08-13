import type { ChatItem } from './types';
import { resolveToolStatus, splitToolTitle } from './MessageBubble';

/**
 * 子 agent 分组纯函数集。
 *
 * ACP 路径下 claude-code 的 Task 子 agent 输出（tool_call / tool_result /
 * assistant_chunk 帧带 `parent_tool_call_id`，父 Task 卡自身 tool_call 帧带
 * `is_subagent`）需要按父子关系收纳进父卡 children 嵌套渲染，主流只保留主 agent
 * 输出。本模块的函数全部纯/幂等，供实时 WS 路径（ChatStream）与历史还原路径
 * （historyToChatItems）共用，保证两条路径渲染结果一致、且可单测。
 */

/** 子 agent Task 卡的元信息（从 toolArgs JSON 防御性提取，Task 工具常见字段）。 */
export interface SubagentMeta {
  /** 头部主标签：优先 description（claude-code Task 工具最常带），
   *  其次 args.name，再次 toolName；全缺省时为 undefined（组件回退 i18n 文案） */
  label?: string;
  /** Task 描述（与 label 同源时冗余，渲染时可略） */
  description?: string;
  /** subagent_type（general-purpose/plan 等），渲染为次要徽标 */
  subagentType?: string;
}

export function extractSubagentMeta(toolArgs?: string, toolName?: string): SubagentMeta {
  let parsed: Record<string, unknown> | null = null;
  if (toolArgs) {
    try {
      const v = JSON.parse(toolArgs) as unknown;
      if (v && typeof v === 'object' && !Array.isArray(v)) {
        parsed = v as Record<string, unknown>;
      }
    } catch {
      /* 坏 JSON 防御：保持 parsed=null */
    }
  }
  const str = (...keys: string[]): string | undefined => {
    if (!parsed) return undefined;
    for (const k of keys) {
      const v = parsed[k];
      if (typeof v === 'string' && v.trim()) return v.trim();
    }
    return undefined;
  };
  const description = str('description');
  const subagentType = str('subagent_type', 'subagentType');
  const label =
    description ??
    str('name', 'task') ??
    (toolName && toolName.trim() ? toolName.trim() : undefined);
  return { label, description, subagentType };
}

/**
 * 把平铺的 ChatItem 列表按 parentToolId 分组：子项（带 parentToolId）嵌套进父
 * 工具卡的 `children`（父卡按 toolId 匹配），顶层只保留无归属项。支持任意嵌套
 * 深度与「父卡后到」时序（第一遍收集 children、第二遍组装）。父卡从未出现
 * （时序异常/深嵌套断链）的子项降级回顶层平铺，保证内容不丢。
 */
export function groupByParent(flat: ChatItem[]): ChatItem[] {
  const childrenByParent = new Map<string, ChatItem[]>();
  const roots: ChatItem[] = [];
  const toolIds = new Set<string>();
  for (const it of flat) {
    if (it.kind === 'tool' && it.toolId) {
      toolIds.add(it.toolId);
      if (!childrenByParent.has(it.toolId)) childrenByParent.set(it.toolId, []);
    }
    if (it.parentToolId) {
      const arr = childrenByParent.get(it.parentToolId) ?? [];
      arr.push(it);
      childrenByParent.set(it.parentToolId, arr);
    } else {
      roots.push(it);
    }
  }
  const assemble = (it: ChatItem): ChatItem => {
    if (it.kind === 'tool' && it.toolId) {
      const kids = childrenByParent.get(it.toolId);
      if (kids && kids.length > 0) {
        return { ...it, children: kids.map(assemble) };
      }
    }
    return it;
  };
  const grouped = roots.map(assemble);
  // 孤儿：parentToolId 指向的父卡从未出现 → 平铺回主流，保证不丢内容
  const orphans: ChatItem[] = [];
  for (const [pid, kids] of childrenByParent) {
    if (!toolIds.has(pid)) orphans.push(...kids);
  }
  return [...grouped, ...orphans];
}

/**
 * 工具卡按 toolId 就地升级（live tool_call 与历史孤儿卡/已收纳子卡去重）：
 * 避免同一工具两张卡（tool_result 只 patch 一张，另一张永远 running）。
 * 语义与 ChatStream 既有 main 工具卡去重一致：
 * - 已带结果（toolResult != null）的卡不降级（乱序重发忽略）；
 * - args 用有意义值合并（`{}`/空占位不覆盖已回填的真实参数）；
 * - 子 agent 父卡保留既有 children，追加迟到挂载的 pending 子项。
 */
export function upsertToolCard(list: ChatItem[], tool: ChatItem): ChatItem[] {
  if (!tool.toolId) return [...list, tool];
  const idx = list.findIndex((it) => it.kind === 'tool' && it.toolId === tool.toolId);
  if (idx < 0) return [...list, tool];
  const cur = list[idx];
  if (cur.toolResult != null) return list;
  const isNoop = (a?: string) => {
    const t = (a ?? '').trim();
    return t === '' || t === '{}';
  };
  const next = [...list];
  next[idx] = {
    ...cur,
    toolName: tool.toolName ?? cur.toolName,
    toolArgs: !isNoop(tool.toolArgs) ? tool.toolArgs : cur.toolArgs,
    toolKind: tool.toolKind ?? cur.toolKind,
    toolStatus: tool.toolStatus ?? 'in_progress',
    toolDiffs: tool.toolDiffs ?? cur.toolDiffs,
    toolLocations: tool.toolLocations ?? cur.toolLocations,
    children:
      tool.children && tool.children.length > 0
        ? [...(cur.children ?? []), ...tool.children]
        : cur.children,
  };
  return next;
}

/** 子 agent 流式文本气泡的定位（children 内下标 + 气泡种类）。 */
export interface ChildStreamState {
  idx: number;
  kind: 'assistant' | 'thought';
}

/**
 * 子 agent 文本 chunk 流式追加：父卡存在时把 content 并入其 children 的当前
 * 流式气泡（同 kind 追加，否则在 children 末尾新建），返回新数组与新的流式气泡
 * 下标。父卡缺失返回 `attached: false`（调用方按孤儿缓存/降级处理）。
 * 纯函数：不读写任何 ref，副作用由调用方按返回值落盘。
 */
export function appendChildStream(
  state: ChatItem[],
  parentToolId: string,
  kind: 'assistant' | 'thought',
  content: string,
  stream: ChildStreamState | null,
): { state: ChatItem[]; stream: ChildStreamState | null; attached: boolean } {
  const parentIdx = state.findIndex((it) => it.kind === 'tool' && it.toolId === parentToolId);
  if (parentIdx < 0) return { state, stream, attached: false };
  const parent = state[parentIdx];
  const children = parent.children ?? [];
  if (stream && children[stream.idx]?.kind === kind) {
    const nextChildren = [...children];
    nextChildren[stream.idx] = {
      ...nextChildren[stream.idx],
      content: nextChildren[stream.idx].content + content,
    };
    const next = [...state];
    next[parentIdx] = { ...parent, children: nextChildren };
    return { state: next, stream, attached: true };
  }
  const next = [...state];
  next[parentIdx] = {
    ...parent,
    children: [...children, { kind, content, parentToolId }],
  };
  return { state: next, stream: { idx: children.length, kind }, attached: true };
}

/**
 * 子 agent 工具结果 patch：在父卡 children 内按 toolId 命中卡片就地更新
 * （args 覆盖规则与主流一致：`{}`/空占位由 tool_result 携带的真实参数补全，
 * 已回填的 args 不被覆盖）。未命中（结果先于调用卡到达的乱序帧）追加一张结果
 * 卡，保证不丢。输入 msg 为 WS tool_result 帧字段的窄类型。
 */
export function patchChildToolResult(
  children: ChatItem[],
  msg: {
    id?: string;
    name?: string;
    result?: string;
    args?: string;
    status?: ChatItem['toolStatus'];
    tool_kind?: ChatItem['toolKind'];
    diffs?: ChatItem['toolDiffs'];
    locations?: ChatItem['toolLocations'];
    parentToolId?: string;
  },
): ChatItem[] {
  const isNoop = (a: string | undefined) => {
    const t = (a ?? '').trim();
    return t === '' || t === '{}';
  };
  if (msg.id) {
    const idx = children.findIndex((it) => it.kind === 'tool' && it.toolId === msg.id);
    if (idx >= 0) {
      const next = [...children];
      const cur = next[idx];
      next[idx] = {
        ...cur,
        toolResult: msg.result,
        toolStatus: msg.status ?? 'completed',
        toolName: cur.toolName ?? msg.name,
        toolArgs: isNoop(cur.toolArgs) && !isNoop(msg.args) ? msg.args : cur.toolArgs ?? msg.args,
        toolKind: cur.toolKind ?? msg.tool_kind,
        toolDiffs: cur.toolDiffs ?? msg.diffs,
        toolLocations: cur.toolLocations ?? msg.locations,
      };
      return next;
    }
  }
  return [
    ...children,
    {
      kind: 'tool',
      content: '',
      toolId: msg.id,
      toolName: msg.name,
      parentToolId: msg.parentToolId,
      toolResult: msg.result,
      toolStatus: msg.status ?? 'completed',
      toolKind: msg.tool_kind,
      toolDiffs: msg.diffs,
      toolLocations: msg.locations,
    },
  ];
}

/** 流式 chunk 攒批键分隔符（parentToolId 内不出现）。 */
export const CHUNK_SEP = '\u0000';

/** 流式 chunk 攒批键：(parentToolId, kind) 分组，避免主/子文本交错串气泡。 */
export function chunkKey(parent: string | undefined, kind: 'assistant' | 'thought'): string {
  return `${parent ?? ''}${CHUNK_SEP}${kind}`;
}

export function parseChunkKey(key: string): { parent: string; kind: 'assistant' | 'thought' } {
  const idx = key.indexOf(CHUNK_SEP);
  const parent = idx >= 0 ? key.slice(0, idx) : '';
  const kind = (idx >= 0 ? key.slice(idx + CHUNK_SEP.length) : 'assistant') as
    | 'assistant'
    | 'thought';
  return { parent, kind };
}

/** 子 agent 固定状态面板的摘要行（从 ChatItem 列表提取，纯函数、可单测）。
 *  status/progress 语义与 SubagentTaskCard 头部完全一致。 */
export interface SubagentSummary {
  /** 在 items 中的下标：虚拟化滚动定位用 */
  index: number;
  toolId?: string;
  /** 头部主标签（extractSubagentMeta().label，可能为空串 → 渲染处回退 i18n） */
  label: string;
  subagentType?: string;
  status: 'pending' | 'in_progress' | 'running' | 'completed' | 'failed';
  /** children 中工具卡数（0 时不显示进度段） */
  toolCount: number;
  /** 最后一个未完成子工具卡的归一化 label（无则 null） */
  runningToolLabel: string | null;
}

/**
 * 提取 items 中所有子 agent 父卡（is_subagent 或带 children 的 tool 卡，判定与
 * ChatStream 渲染 SubagentTaskCard 的条件一致）的状态摘要，供固定状态面板渲染。
 * 进度语义与 SubagentTaskCard 相同：运行中显示「N 工具 · 当前工具」，已完成仅
 * 「N 工具」；无子工具卡则无进度段。
 */
export function collectSubagents(items: ChatItem[]): SubagentSummary[] {
  const out: SubagentSummary[] = [];
  for (let i = 0; i < items.length; i++) {
    const it = items[i];
    if (it.kind !== 'tool') continue;
    if (!(it.isSubagent || (it.children && it.children.length > 0))) continue;
    const meta = extractSubagentMeta(it.toolArgs, it.toolName);
    const tools = (it.children ?? []).filter((c) => c.kind === 'tool');
    const running = [...tools]
      .reverse()
      .find((c) => {
        const s = resolveToolStatus(c);
        return s !== 'completed' && s !== 'failed';
      });
    out.push({
      index: i,
      toolId: it.toolId,
      label: meta.label ?? '',
      subagentType: meta.subagentType,
      status: resolveToolStatus(it),
      toolCount: tools.length,
      runningToolLabel: running
        ? splitToolTitle(running.toolName, running.toolKind).label
        : null,
    });
  }
  return out;
}
