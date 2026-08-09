import type { ApprovalOption } from '../../types';

/** 聊天区单条消息。 */
export interface ChatItem {
  kind: 'user' | 'assistant' | 'tool' | 'approval' | 'thought' | 'plan';
  content: string;
  toolName?: string;
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
