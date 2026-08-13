import type { AgentMessage } from '../../types';
import type { ChatItem, ToolDiff, ToolKind, ToolLocation } from './types';
import { parseAcpToolJson, parsePlanEntries } from './types';
import { groupByParent } from './subagent';

interface CallRecord {
  name: string;
  args: string;
  toolKind?: ToolKind;
  toolDiffs?: ToolDiff[];
  toolLocations?: ToolLocation[];
  /** 子 agent 归属：该调用由哪个 Task 父卡发起（tool_calls JSON 元素或行列） */
  parentToolId?: string;
}

/** 压缩重插去重：kept 段在 summary 前保留原始行（801c9a6），DB 物理顺序为
 *  [..., 原kept, summary, 重插kept, 压缩后新消息...]，前端全量渲染会重复。
 *  不能用「summary 之后的行数」当作重插行数（压缩后新消息也排在 summary 后，
 *  会把行数放大、多跳掉没有重复副本的合法旧行）。改为内容匹配：对每个 summary，
 *  以「summary 后紧跟的重插段」为模板，从 summary 前紧邻行向前找等长且逐行全等
 *  （kind/role/content/tool_calls/tool_call_id/name）的连续段——重插段是 kept 段
 *  原样复制，故 summary 前必存在这样一段原件。
 *
 *  导出供分页「加载更早消息」复用：新页 + 已加载页拼成完整集合算 skip，再把落在
 *  新页范围内的下标过滤出来，跳过跨页压缩重插的原件（见 ChatStream 的 prepend）。 */
export function compactionSkippedIndices(history: AgentMessage[]): Set<number> {
  const normNull = (v: unknown) => (v === undefined ? null : v);
  const rowEquals = (a: AgentMessage, b: AgentMessage) =>
    a.kind === b.kind &&
    a.role === b.role &&
    a.content === b.content &&
    normNull(a.tool_calls) === normNull(b.tool_calls) &&
    normNull(a.tool_call_id) === normNull(b.tool_call_id) &&
    normNull(a.name) === normNull(b.name) &&
    normNull(a.parent_tool_call_id) === normNull(b.parent_tool_call_id);
  const skipBeforeSummary = new Set<number>();
  for (let s = 0; s < history.length; s++) {
    if (history[s].kind !== 'summary') continue;
    // summary 后、到下一个 summary（或数组末尾）之间的行数 = 重插段长上界
    let upper = 0;
    while (s + 1 + upper < history.length && history[s + 1 + upper].kind !== 'summary') upper++;
    // 从长到短尝试：找到「summary 前紧邻 len 行」与「summary 后前 len 行」全等的最大 len
    let matched = 0;
    for (let len = Math.min(upper, s); len >= 1; len--) {
      let all = true;
      for (let m = 0; m < len; m++) {
        if (!rowEquals(history[s - len + m], history[s + 1 + m])) {
          all = false;
          break;
        }
      }
      if (all) {
        matched = len;
        break;
      }
    }
    for (let m = 0; m < matched; m++) {
      skipBeforeSummary.add(s - matched + m);
    }
  }
  return skipBeforeSummary;
}

/** 历史装载（纯函数）：把服务端 DB 历史行转换为聊天区 ChatItem 列表。
 *
 *  关键去重——同一 tool_call_id 只出一张工具卡：服务端历史上对同一
 *  tool_call_id 可能落库多行（1 条 kind='tool_calls' + N 条 kind='tool_result'，
 *  ACP 的 ToolCallUpdate/ToolResult 中间态 content 为空）。旧逻辑每行渲染一张卡
 *  导致同一工具多张卡、live 匹配只 patch 第一张、其余残留 running。
 *  - kind='tool_result' 且 tool_call_id 非空：先扫一遍构建 resultById（content
 *    非空者优先、同等取后者）；渲染时每个 id 只用 resultById 里的终态行。
 *  - kind='tool_calls' 有配对 tool_result 时跳过（args 已由 tool_result 卡片展示），
 *    无配对（回合中断在工具执行中）才渲染 failed 占位卡。
 *  - 旧格式合并行（kind==='tool'||role==='tool'）与 runner 旧格式（tool_call_id
 *    列为空、JSON 内带 id）没有 tool_call_id 列值可去重，走原路径。 */
export function historyToChatItems(history: AgentMessage[]): ChatItem[] {
  return historyToChatItemsWithSkip(history, compactionSkippedIndices(history));
}

/** 分页「加载更早」去重：对「新页 + 已加载页」的完整集合算压缩重插 skip，再只取
 *  落在新页范围内的下标。跨页压缩重插（kept 段原样复制）的原件若在新页、重插副本
 *  在已加载页，只有在此完整集合上匹配才跳得掉；已加载页内的 skip 在首次装载时
 *  已生效，忽略。返回可直接传给 `historyToChatItemsWithSkip(newRows, ...)` 的集合。 */
export function prependSkip(newRows: AgentMessage[], loadedRows: AgentMessage[]): Set<number> {
  const combined = [...newRows, ...loadedRows];
  const skip = compactionSkippedIndices(combined);
  const local = new Set<number>();
  for (const i of skip) {
    if (i < newRows.length) local.add(i);
  }
  return local;
}

/** 带预计算 skip 集合的转换：供分页「加载更早」使用。`skip` 是对 `history` 的
 *  压缩重插去重下标集（通常由 `compactionSkippedIndices` 在**完整已加载集合**上
 *  计算后过滤出本页范围），与 `historyToChatItems` 内部自动计算的语义一致。 */
export function historyToChatItemsWithSkip(
  history: AgentMessage[],
  skip: Set<number>,
): ChatItem[] {
  // 新格式：kind='tool_calls' 行的原始调用记录，按 tool_call_id 关联 args；
  // 同时保留 ACP 新格式的 tool_kind/diffs/locations 供 tool_result 行合并
  const callArgs = new Map<string, CallRecord>();
  for (const m of history) {
    if (m.kind === 'tool_calls' && m.tool_calls) {
      try {
        const parsed = JSON.parse(m.tool_calls) as {
          id: string;
          function?: { name?: string; arguments?: string };
          parent_tool_call_id?: string | null;
        }[];
        const acp = parseAcpToolJson(m.tool_calls);
        for (const c of parsed) {
          callArgs.set(c.id, {
            // ACP 新格式：name/arguments 平铺；runner 旧格式：function 嵌套
            name:
              (c as { name?: string }).name ?? c.function?.name ?? m.name ?? '',
            args:
              (c as { arguments?: string }).arguments ?? c.function?.arguments ?? '',
            ...acp,
            // 子 agent 归属：JSON 元素优先（子调用自身），回退行级列
            parentToolId: c.parent_tool_call_id ?? m.parent_tool_call_id ?? undefined,
          });
        }
      } catch {
        /* ignore malformed tool_calls */
      }
    }
  }
  // 终态 tool_result 去重表：同一 tool_call_id 只保留一条——content 非空者优先、
  // 同等取后者（中间态空 content 行被最终结果覆盖）。渲染时每个 id 只用此表。
  const resultById = new Map<string, AgentMessage>();
  for (const m of history) {
    if (m.kind !== 'tool_result' || !m.tool_call_id) continue;
    const prev = resultById.get(m.tool_call_id);
    if (!prev) {
      resultById.set(m.tool_call_id, m);
      continue;
    }
    const prevHasContent = prev.content !== '';
    const curHasContent = m.content !== '';
    if (curHasContent && !prevHasContent) {
      resultById.set(m.tool_call_id, m); // 非空覆盖空
    } else if (curHasContent === prevHasContent) {
      resultById.set(m.tool_call_id, m); // 同等取后者
    }
  }
  const loaded: ChatItem[] = [];
  // 历史中多条 plan 行只保留最后一条（ACP plan 全量替换语义）：先记录索引
  let lastPlanIdx = -1;
  for (let i = 0; i < history.length; i++) {
    const m = history[i];
    if (skip.has(i)) continue;
    if (m.kind === 'tool_result') {
      // 同一 tool_call_id 只渲染 resultById 里的终态行（中间态空 content 行跳过）；
      // tool_call_id 为空的旧数据无 id 可去重，原样渲染。
      if (m.tool_call_id && resultById.get(m.tool_call_id) !== m) continue;
      const call = m.tool_call_id ? callArgs.get(m.tool_call_id) : undefined;
      loaded.push({
        kind: 'tool',
        content: '',
        toolName: call?.name ?? m.name ?? '',
        toolId: m.tool_call_id ?? undefined,
        toolArgs: call?.args ?? '',
        toolResult: m.content,
        toolStatus: 'completed',
        toolKind: call?.toolKind,
        toolDiffs: call?.toolDiffs,
        toolLocations: call?.toolLocations,
        parentToolId: call?.parentToolId ?? m.parent_tool_call_id ?? undefined,
      });
    } else if ((m.kind === 'tool' || m.role === 'tool') && m.tool_calls) {
      // 旧格式：合并 tool_log JSON 行
      try {
        for (const t of JSON.parse(m.tool_calls)) {
          loaded.push({ kind: 'tool', content: '', toolName: t.name, toolArgs: t.args, toolResult: t.result });
        }
      } catch {
        /* ignore malformed tool_calls */
      }
    } else if (m.kind === 'tool_calls') {
      // kind='tool_calls' 行有配对 tool_result 时跳过（args 已由 tool_result 卡片
      // 携带，重复渲染会产生孤儿 failed 卡）。无配对（回合中断在工具执行中：
      // ToolCall 已落库，ToolCallUpdate/tool_result 永不到达）渲染 failed 占位卡，
      // 否则该工具从聊天区彻底消失（现象：卡片无标题无内容、或凭空少一段）。
      if (m.tool_call_id && !m.content && !resultById.has(m.tool_call_id)) {
        const call = callArgs.get(m.tool_call_id);
        if (call) {
          loaded.push({
            kind: 'tool',
            content: '',
            toolName: call.name,
            toolId: m.tool_call_id,
            toolArgs: call.args,
            toolResult: undefined,
            toolStatus: 'failed',
            toolKind: call.toolKind,
            toolDiffs: call.toolDiffs,
            toolLocations: call.toolLocations,
            parentToolId: call.parentToolId ?? m.parent_tool_call_id ?? undefined,
          });
        }
      } else if (!m.tool_call_id && m.tool_calls) {
        // runner 旧格式：整行 tool_call_id 列为空，但 JSON 内每个调用带 id。
        // 按 id 与 tool_result 行配对——未配对的（回合在工具执行中被取消）也
        // 渲染 failed 占位卡，否则这些工具刷新后从聊天区消失。
        try {
          for (const c of JSON.parse(m.tool_calls) as { id?: string }[]) {
            if (!c.id || resultById.has(c.id)) continue;
            const call = callArgs.get(c.id);
            if (call) {
              loaded.push({
                kind: 'tool',
                content: '',
                toolName: call.name,
                toolId: c.id,
                toolArgs: call.args,
                toolResult: undefined,
                toolStatus: 'failed',
                toolKind: call.toolKind,
                toolDiffs: call.toolDiffs,
                toolLocations: call.toolLocations,
                parentToolId: call.parentToolId ?? m.parent_tool_call_id ?? undefined,
              });
            }
          }
        } catch {
          /* ignore malformed tool_calls */
        }
      }
    } else if (m.kind === 'message' && m.name === 'thought' && m.content) {
      loaded.push({
        kind: 'thought',
        content: m.content,
        parentToolId: m.parent_tool_call_id ?? undefined,
      });
    } else if (m.kind === 'message' && m.name === 'plan') {
      // 只保留最后一条 plan（ACP plan 全量替换语义）：先记录索引，循环后处理
      lastPlanIdx = loaded.length;
      loaded.push({ kind: 'plan', content: '', planEntries: parsePlanEntries(m.content) });
    } else if (m.kind === 'message' && m.content) {
      // 子 agent 文本：仅 assistant 行带归属（user 消息永远主 agent 层）
      loaded.push({
        kind: m.role === 'user' ? 'user' : 'assistant',
        content: m.content,
        parentToolId:
          m.role === 'user' ? undefined : (m.parent_tool_call_id ?? undefined),
      });
    } else if (m.kind === 'summary' && m.content) {
      // summary 渲染为 assistant 气泡（muted 样式），避免与普通用户消息混淆
      loaded.push({ kind: 'assistant', content: m.content });
    }
    // kind='tool_calls' 行本身不渲染（args 已合并进 tool_result 卡片）
  }
  // 历史中多条 plan 行只渲染最后一条
  const flat =
    lastPlanIdx >= 0
      ? loaded.filter((it, i) => it.kind !== 'plan' || i === lastPlanIdx)
      : loaded;
  // 按 parentToolId 分组嵌套：子 agent 的 tool/text/thought 收进父 Task 卡 children，
  // 与实时 WS 路径渲染结果一致（groupByParent 对孤儿项平铺回主流，内容不丢）。
  return groupByParent(flat).map((it) =>
    // 子 agent 父卡有 children 但无 result 时（刷新打断，服务端可能仍在跑），
    // failed 占位改 in_progress，避免「有进度却标失败」的矛盾展示
    it.kind === 'tool' && (it.children?.length ?? 0) > 0 && it.toolResult == null && it.toolStatus === 'failed'
      ? { ...it, toolStatus: 'in_progress' }
      : it,
  );
}
