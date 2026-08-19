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

/** subagent 类型徽标的展示元信息。 */
export interface SubagentTypeMeta {
  /** 本地化显示名的 i18n key；缺省时组件直接用原始 subagent_type 原值 */
  labelKey?: string;
  /** 淡色圆角底（chip 底色，light/dark 双配，与 KindChip 同构） */
  chipClass: string;
  /** 彩色文字 */
  textClass: string;
}

/** subagent_type → 展示元信息。claude-code Task 子代理常见类型各配语义色，
 *  与工具卡 KindChip 的「淡色底 + 彩色文字」语言一致：explore（只读探索）→
 *  teal（同 search 工具色）；general-purpose（全能助手）→ slate 中性灰；
 *  plan（规划方案）→ violet（同 think 工具色）。未知类型回退 muted 灰 + 原值
 *  显示，不翻译、不抢视觉。
 *  `as const satisfies` 保留 labelKey 的字面量类型，让 SubagentTypeBadge 里
 *  `t(meta.labelKey)` 通过 i18next 的强类型 key 校验。 */
const SUBAGENT_TYPE_STYLE = {
  explore: {
    labelKey: 'agent.subagentTypeExplore',
    chipClass: 'bg-teal-500/10',
    textClass: 'text-teal-600 dark:text-teal-400',
  },
  'general-purpose': {
    labelKey: 'agent.subagentTypeGeneral',
    chipClass: 'bg-slate-500/10',
    textClass: 'text-slate-600 dark:text-slate-400',
  },
  plan: {
    labelKey: 'agent.subagentTypePlan',
    chipClass: 'bg-violet-500/10',
    textClass: 'text-violet-600 dark:text-violet-400',
  },
} as const satisfies Record<string, SubagentTypeMeta>;

/** 已知 subagent 类型 → 语义色元信息（labelKey 为字面量联合，供 t() 强类型）；
 *  未知类型返回 muted 灰 + labelKey 缺省（组件回退显示原值）。空/未定义返回
 *  undefined（调用方不渲染徽标）。 */
export function subagentTypeMeta(type?: string) {
  if (!type) return undefined;
  // as const 对象无 string 索引签名：运行时未知 key 返回 undefined 走回退，
  // keyof 断言保留 labelKey 字面量类型（供 t() 强类型校验）
  const key = type.trim().toLowerCase() as keyof typeof SUBAGENT_TYPE_STYLE;
  return (
    SUBAGENT_TYPE_STYLE[key] ?? {
      labelKey: undefined,
      chipClass: 'bg-muted',
      textClass: 'text-muted-foreground',
    }
  );
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

/** mergePages 的返回：合并后的列表 + 被吸收的孤儿在已加载页中的下标（升序）。
 *  调用方据此修正流式气泡 ref 的位移（孤儿从顶层移除后，其后各项下标额外 -1）。 */
export interface MergePagesResult {
  items: ChatItem[];
  absorbedIndexes: number[];
}

/**
 * 分页「加载更早」的跨页孤儿重归组：更早一页（`older`，含父 Task 卡）到达时，
 * 把已加载页（`loaded`）中按 `parentToolId` 指向这些父卡的顶层孤儿子项收进父卡
 * children，与「父卡后到」的实时路径（orphan 收纳）行为一致。
 *
 * 时序背景：分页加载时父 Task 卡的 tool_calls 行可能落在更早页、而其子项
 * （parent_tool_call_id 指向父卡 id 的 tool/text/thought 行）在已加载页。已加载页
 * 首次转换时父卡缺席，子项经 groupByParent 降级为顶层孤儿平铺；父卡随更早页并入后
 * 必须把它们收回 children，否则子项永久停留在主流（跨页孤儿 Bug）。
 *
 * - 只移动顶层孤儿（`loaded` 内带 parentToolId 且命中 `older` 顶层父卡 toolId 的项）；
 * - 孤儿按到达顺序追加到父卡既有 children 之后（子项在更早页之后落库，chronologically 靠后）；
 * - 父卡在 `older` 内嵌套（本身是更早层父卡的 children）时同样按递归命中；
 * - 返回被吸收孤儿在 `loaded` 中的下标，供调用方修正 streamingIdxRef 等下标位移。
 */
export function mergePages(older: ChatItem[], loaded: ChatItem[]): MergePagesResult {
  // 更早页中出现的父卡 toolId 集合（含嵌套在 children 里的父卡）
  const parentIds = new Set<string>();
  const collectIds = (it: ChatItem) => {
    if (it.kind === 'tool' && it.toolId) parentIds.add(it.toolId);
    for (const c of it.children ?? []) collectIds(c);
  };
  for (const it of older) collectIds(it);
  if (parentIds.size === 0) return { items: [...older, ...loaded], absorbedIndexes: [] };

  const orphans: ChatItem[] = [];
  const absorbedIndexes: number[] = [];
  const rest: ChatItem[] = [];
  for (let i = 0; i < loaded.length; i++) {
    const it = loaded[i];
    if (it.parentToolId && parentIds.has(it.parentToolId)) {
      orphans.push(it);
      absorbedIndexes.push(i);
    } else {
      rest.push(it);
    }
  }
  if (orphans.length === 0) return { items: [...older, ...loaded], absorbedIndexes: [] };

  // 把孤儿按 parentToolId 收进对应父卡 children（递归命中嵌套父卡）
  const attach = (it: ChatItem): ChatItem => {
    const children = (it.children ?? []).map(attach);
    if (it.kind === 'tool' && it.toolId) {
      const kids = orphans.filter((o) => o.parentToolId === it.toolId);
      if (kids.length > 0) return { ...it, children: [...children, ...kids] };
    }
    if (it.children && it.children.length > 0) return { ...it, children };
    return it;
  };
  return { items: [...older.map(attach), ...rest], absorbedIndexes };
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

/** 流式 tool_call_chunk 占位卡的合成 toolId 前缀（无 id 时按 index 归位）。 */
export const STREAM_TOOL_ID_PREFIX = '__stream_';

/** 递归清理 tool 卡 children 内残留的流式占位卡（toolId 以 STREAM_TOOL_ID_PREFIX
 *  开头的卡）。顶层及所有嵌套 children 均处理，保证 done/stream_reset/tool_call 子
 *  agent 分支只需一次调用即可清除所有层级的合成卡。 */
export function dropStreamPlaceholders(list: ChatItem[]): ChatItem[] {
  return list
    .filter((it) => !(it.kind === 'tool' && it.toolId && it.toolId.startsWith(STREAM_TOOL_ID_PREFIX)))
    .map((it) => (it.children ? { ...it, children: dropStreamPlaceholders(it.children) } : it));
}

/**
 * tool_call_chunk 帧（runner 路径工具参数流式透出）→ 占位卡就地更新：
 * 参数增量只累计不渲染全文（正式 tool_call 帧到达后经 upsertToolCard 替换）。
 * 同一 index 的占位卡按真实 id（到达后迁移）或合成键 `__stream_{index}` 匹配；
 * 新建占位卡用 in_progress 状态渲染「正在调用 <name>…」。
 */
export function applyToolCallChunk(
  list: ChatItem[],
  msg: {
    index?: number;
    id?: string;
    name?: string;
    arguments?: string;
    parent_tool_call_id?: string;
  },
): ChatItem[] {
  // 有 parent_tool_call_id 时：路由进父卡 children 递归处理
  if (msg.parent_tool_call_id) {
    const parentIdx = list.findIndex(
      (it) => it.kind === 'tool' && it.toolId === msg.parent_tool_call_id,
    );
    if (parentIdx >= 0) {
      const parent = list[parentIdx];
      const childChunk = {
        index: msg.index,
        id: msg.id,
        name: msg.name,
        arguments: msg.arguments,
      };
      const nextChildren = applyToolCallChunk(parent.children ?? [], childChunk);
      const next = [...list];
      next[parentIdx] = { ...parent, children: nextChildren };
      return next;
    }
    // 找不到父卡：帧先于父卡到达，降级到主流（后续 groupByParent 会归位）
  }

  const index = msg.index ?? 0;
  const synthetic = `${STREAM_TOOL_ID_PREFIX}${index}`;
  const toolId = msg.id ?? synthetic;

  // 优先按真实 id 找，找不到再按合成键找（id 后到达的迁移中间态）
  let idx = list.findIndex((it) => it.kind === 'tool' && it.toolId === (msg.id ?? synthetic));
  if (idx < 0 && msg.id) {
    idx = list.findIndex((it) => it.kind === 'tool' && it.toolId === synthetic);
  }
  const next = [...list];
  if (idx < 0) {
    next.push({
      kind: 'tool',
      content: '',
      toolId,
      toolName: msg.name,
      toolArgs: msg.arguments,
      toolStatus: 'in_progress',
    });
    return next;
  }
  const cur = next[idx];
  next[idx] = {
    ...cur,
    toolId,
    toolName: msg.name ?? cur.toolName,
    toolArgs: (cur.toolArgs ?? '') + (msg.arguments ?? ''),
  };
  return next;
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
