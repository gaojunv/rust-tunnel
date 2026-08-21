import type { ApprovalOption, ElicitationRequestSchema } from '../../types';

/** 聊天区单条消息。 */
export interface ChatItem {
  kind: 'user' | 'assistant' | 'tool' | 'approval' | 'elicitation' | 'thought' | 'plan' | 'system' | 'attachment';
  content: string;
  /** 稳定身份，供 React key 使用（渲染时 `id ?? index`）：
   *  history 装载的行用服务端 rowid（AgentMessage.id），live WS 帧创建的气泡
   *  （流式 assistant/thought、user、system、plan 等）在创建时分配客户端自增 id。
   *  流式追加/终态只改 content 不改 id；`stream_reset` 移除半截流式气泡后其余项
   *  key 不漂移（index 位移会导致后续气泡重挂载，丢 ToolCard/ThoughtBubble 展开态）。 */
  id?: string;
  /** kind='system'：提示行语气（状态/警告/错误/停止），缺省按 info 渲染 */
  systemTone?: 'info' | 'warning' | 'error' | 'stopped';
  toolName?: string;
  /** 工具调用稳定身份（tool_call_id）：history 装载与 live WS 帧按它去重/匹配，
   *  防止刷新后同一工具被渲染成两张卡（一张历史孤儿、一张 live 追加）。 */
  toolId?: string;
  toolArgs?: string;
  toolResult?: string;
  /** ACP 工具分类（图标/详情渲染依据）；runner 旧数据无此字段 */
  toolKind?: ToolKind;
  /** 工具执行状态：缺省（旧帧/历史 runner 数据）按 result 有无推断 */
  toolStatus?: 'pending' | 'in_progress' | 'running' | 'completed' | 'failed';
  /** 文件修改 diff（edit/delete/move 类工具） */
  toolDiffs?: ToolDiff[];
  /** 涉及的文件位置 */
  toolLocations?: ToolLocation[];
  /** kind='plan'：计划条目 */
  planEntries?: PlanEntryItem[];
  /** kind='approval'：审批卡片 */
  approvalId?: string;
  approvalTool?: string;
  approvalSummary?: string;
  /** ACP 权限选项透传（有则渲染选项按钮，无则 approve/deny 二元按钮） */
  approvalOptions?: ApprovalOption[];
  /** pending=等待用户响应；approved/denied=用户主动处理；expired=回合终态被动过期 */
  approvalStatus?: 'pending' | 'approved' | 'denied' | 'expired';
  /** 审批预览原始文本（edit/write 格式可解析为 diff 展示） */
  approvalArgsPreview?: string;
  /** kind='elicitation'：用户表单卡（AskUserQuestion / MCP elicitation / refusal-fallback） */
  elicitationId?: string;
  elicitationMessage?: string;
  elicitationSchema?: ElicitationRequestSchema;
  /** pending=等待用户填表；accepted/declined=用户主动处理；cancelled=回合终态/断线/超时被动取消 */
  elicitationStatus?: 'pending' | 'accepted' | 'declined' | 'cancelled';
  /** 子 agent 归属：该条消息属于某个 Task 子 agent（值为父卡的 toolId） */
  parentToolId?: string;
  /** 子 agent 父卡标记：is_subagent=true 的 tool_call 帧（Task 卡本身） */
  isSubagent?: boolean;
  /** 子 agent 父卡的嵌套子项（工具卡/文本/思考气泡按到达顺序收纳） */
  children?: ChatItem[];
  /** kind='attachment'：ACP 多模态占位卡（image/audio/resource），只带元信息 */
  attachmentKind?: 'image' | 'audio' | 'resource' | string;
  attachmentName?: string;
  attachmentUri?: string;
  attachmentMime?: string;
}

export interface ToolDiff {
  path: string;
  old_text: string | null;
  new_text: string | null;
}

export interface ToolLocation {
  path: string;
  line?: number | null;
}

export interface PlanEntryItem {
  content: string;
  status: 'pending' | 'in_progress' | 'completed';
  priority?: 'high' | 'medium' | 'low';
}

export const TOOL_KINDS = [
  'read', 'edit', 'delete', 'move', 'search', 'execute',
  'think', 'fetch', 'switch_mode', 'other',
] as const;
export type ToolKind = (typeof TOOL_KINDS)[number];

function asToolKind(v: unknown): ToolKind | undefined {
  return typeof v === 'string' && (TOOL_KINDS as readonly string[]).includes(v)
    ? (v as ToolKind)
    : undefined;
}

function asDiffs(v: unknown): ToolDiff[] | undefined {
  if (!Array.isArray(v) || v.length === 0) return undefined;
  const diffs: ToolDiff[] = [];
  for (const d of v) {
    if (d && typeof d === 'object' && typeof (d as ToolDiff).path === 'string') {
      const rec = d as Record<string, unknown>;
      diffs.push({
        path: rec.path as string,
        old_text: typeof rec.old_text === 'string' ? rec.old_text : null,
        new_text: typeof rec.new_text === 'string' ? rec.new_text : null,
      });
    }
  }
  return diffs.length > 0 ? diffs : undefined;
}

function asLocations(v: unknown): ToolLocation[] | undefined {
  if (!Array.isArray(v) || v.length === 0) return undefined;
  const locations: ToolLocation[] = [];
  for (const l of v) {
    if (l && typeof l === 'object' && typeof (l as ToolLocation).path === 'string') {
      const rec = l as Record<string, unknown>;
      locations.push({
        path: rec.path as string,
        line: typeof rec.line === 'number' ? rec.line : null,
      });
    }
  }
  return locations.length > 0 ? locations : undefined;
}

/** 解析历史行 tool_calls 列的规范化 JSON（ACP 落库格式）。
 *  旧 runner 格式（function.arguments 嵌套）与坏 JSON 一律容错返回 {}。 */
export function parseAcpToolJson(json: string): {
  toolKind?: ToolKind;
  toolDiffs?: ToolDiff[];
  toolLocations?: ToolLocation[];
} {
  try {
    const arr = JSON.parse(json) as unknown;
    if (!Array.isArray(arr) || arr.length === 0) return {};
    const first = arr[0] as Record<string, unknown>;
    if (!first || typeof first !== 'object') return {};
    const out: ReturnType<typeof parseAcpToolJson> = {};
    const kind = asToolKind(first.tool_kind);
    if (kind) out.toolKind = kind;
    const diffs = asDiffs(first.diffs);
    if (diffs) out.toolDiffs = diffs;
    const locations = asLocations(first.locations);
    if (locations) out.toolLocations = locations;
    return out;
  } catch {
    return {};
  }
}

const TOOL_STATUSES = ['pending', 'in_progress', 'running', 'completed', 'failed'] as const;

function asToolStatus(v: unknown): ChatItem['toolStatus'] | undefined {
  return typeof v === 'string' && (TOOL_STATUSES as readonly string[]).includes(v)
    ? (v as ChatItem['toolStatus'])
    : undefined;
}

/** kind='tool_result' 行 content 结构化解析结果（服务端新契约，见
 *  `parseToolResultContent`）。空字段省略。 */
export interface ParsedToolResult {
  text: string;
  status?: ChatItem['toolStatus'];
  diffs?: ToolDiff[];
  locations?: ToolLocation[];
}

/** 解析 kind='tool_result' 行 content：服务端新契约把它从纯文本升级为 JSON
 *  `{"text": string, "status"?: string, "diffs"?: ToolDiff[], "locations"?: ToolLocation[]}`
 *  （空字段省略；status 为 running/completed/failed 等）。向后兼容：存量旧行仍是
 *  纯文本——解析失败或 JSON 不含 string 型 `text`（非该结构）一律按纯文本原样返回。
 *  导出供 history.ts（历史还原卡片）与 GitPanel（git_status 回退展示）等消费。 */
export function parseToolResultContent(content: string): ParsedToolResult {
  if (!content) return { text: content };
  try {
    const v: unknown = JSON.parse(content);
    if (v && typeof v === 'object' && !Array.isArray(v)) {
      const obj = v as Record<string, unknown>;
      if (typeof obj.text === 'string') {
        return {
          text: obj.text,
          status: asToolStatus(obj.status),
          diffs: asDiffs(obj.diffs),
          locations: asLocations(obj.locations),
        };
      }
    }
  } catch {
    /* 非 JSON → 纯文本旧格式 */
  }
  return { text: content };
}

/** 解析历史行 plan content（entries JSON 文本）。 */
export function parsePlanEntries(json: string): PlanEntryItem[] {
  try {
    const arr = JSON.parse(json) as unknown;
    if (!Array.isArray(arr)) return [];
    return arr
      .filter((e): e is Record<string, unknown> => !!e && typeof e === 'object')
      .map((e) => ({
        content: typeof e.content === 'string' ? e.content : '',
        status:
          e.status === 'completed' || e.status === 'in_progress' ? e.status : 'pending',
      }));
  } catch {
    return [];
  }
}
