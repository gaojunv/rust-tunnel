import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useVirtualizer } from '@tanstack/react-virtual';
import { Button } from '@/components/ui/button';
import { Loader2, SendHorizontal, Square } from 'lucide-react';
import { useMediaQuery } from '@/hooks/useMediaQuery';
import { useImeGuard } from '@/hooks/useImeGuard';
import {
  agentWsUrl,
  getApiErrorMessage,
  listAgentMessages,
  updateAgentSessionModel,
} from '../../api/client';
import type { AgentMessagesPage } from '../../api/client';
import { useRoles } from '../../api/hooks';
import type { AgentMessage, AgentRole, AgentSession, AgentWsEvent, TodoItem } from '../../types';
import type { ChatItem } from './types';
import ApprovalCard from './ApprovalCard';
import ElicitationCard from './ElicitationCard';
import MentionPopup from './MentionPopup';
import type { SlashCommand } from './SlashCommandPopup';
import SlashCommandPopup from './SlashCommandPopup';
import MessageBubble from './MessageBubble';
import SessionSettingsMenu from './SessionSettingsMenu';
import SubagentTaskCard from './SubagentTaskCard';
import SubagentPanel from './SubagentPanel';
import SystemMessage from './SystemMessage';
import ConfigOptionButton from './ConfigOptionButton';
import { normalizeConfigOptions, optionValue, restoreConfigValue } from './sessionConfig';
import {
  compactionSkippedIndices,
  historyToChatItems,
  historyToChatItemsWithSkip,
  prependSkip,
} from './history';
import {
  appendChildStream,
  applyToolCallChunk,
  chunkKey,
  collectSubagents,
  dropStreamPlaceholders,
  mergePages,
  parseChunkKey,
  patchChildToolResult,
  STREAM_TOOL_ID_PREFIX,
  upsertToolCard,
} from './subagent';
import type { SessionConfigOption } from '../../types';

const RUNNING_TIMEOUT_MS = 10 * 60 * 1000; // 10 分钟不活动兜底（每帧回合活动重置，非回合总时长）
/** 计入「回合活动」的 WS 帧类型：到达即重置 running 不活动兜底倒计时。
 *  配置/标题类帧（session_state/config_option_update/current_mode_update/
 *  session_title/queued）可能由无关操作（如另一标签页切配置）触发，不代表
 *  本回合在推进，不计入。 */
const TURN_ACTIVITY_TYPES = new Set([
  'assistant_chunk',
  'stream_reset',
  'tool_call',
  'tool_call_chunk',
  'tool_result',
  'plan',
  'usage',
  'status',
  'approval_request',
  'elicitation_request',
]);
/** 流式 chunk 合并 flush 间隔：token 级 WS 帧攒批后一次性写 state，避免每 token 全列表重渲染。 */
export const STREAM_FLUSH_MS = 50;
/** 分页「加载更早」每页条数（与后端默认 limit 一致）。 */
const EARLIER_PAGE_SIZE = 200;
/** 连接假死判定阈值：服务端应用层心跳每 25s 一帧，连续 3 个心跳周期（75s）
 *  无任何帧即认为连接被中间设备静默掐断（半开 TCP 不触发 onclose），由看门狗
 *  主动 close 走既有 onclose 重连路径。 */
const HEARTBEAT_TIMEOUT_MS = 75_000;
/** 看门狗扫描周期：远小于心跳超时，保证假死判定延迟在可接受范围。 */
const WATCHDOG_INTERVAL_MS = 30_000;
/** live WS 帧创建消息的稳定 id 计数器：history 行用服务端 rowid（AgentMessage.id），
 *  live 创建的流式气泡/user/system/plan 消息在创建时分配 `live-N` 唯一 id（React key
 *  用）。模块级计数跨组件挂载/多标签页递增，与 DB rowid（数字串）格式不冲突。
 *  stream_reset 移除半截流式气泡后其余项 key 不漂移。 */
let liveItemSeq = 0;
function nextLiveItemId(): string {
  liveItemSeq += 1;
  return `live-${liveItemSeq}`;
}

/** 历史查询数据归一化：兼容 `{ messages, has_more }`（新 API）与裸数组
 * （旧缓存 / 测试替身）。取消息行数组。 */
function historyRows(h: AgentMessagesPage | AgentMessage[] | undefined): AgentMessage[] {
  if (!h) return [];
  return Array.isArray(h) ? h : (h.messages ?? []);
}

/** 历史查询数据的 has_more（裸数组旧形态恒为 false）。 */
function historyHasMore(h: AgentMessagesPage | AgentMessage[] | undefined): boolean {
  return !Array.isArray(h) && (h?.has_more ?? false);
}

interface Props {
  sessionId: string;
  workspaceId: string;
  model: string;
  approvalMode?: string;
  onModelChange: (id: string) => void;
  /** 多标签页模式下当前是否激活（hidden → 可见切换时对齐底部）。缺省视为激活。 */
  active?: boolean;
}

export default function ChatStream({ sessionId, workspaceId, model, approvalMode: initialApprovalMode, onModelChange, active }: Props) {
  const { t } = useTranslation();
  // t 的身份随语言切换变化，把它放进 WS effect 的依赖会导致切语言时拆断
  // 进行中的回合（onclose 追加"连接中断"气泡、过期所有 pending 审批、
  // 触发全量历史重载）。用 ref 存最新 t，effect 从 ref 读，保持引用稳定。
  const tRef = useRef(t);
  useEffect(() => {
    tRef.current = t;
  }, [t]);
  const queryClient = useQueryClient();
  const [items, setItems] = useState<ChatItem[]>([]);
  // 响应式分支：桌面端（≥768px）右侧固定栏，移动端顶部固定面板。
  // jsdom/SSR 无 matchMedia → 恒 false（测试环境走 top 形态）。
  const isDesktop = useMediaQuery('(min-width: 768px)');
  // subagent 固定面板数据源：从消息流提取子代理父卡摘要（纯函数，items 变化时重算）
  const subagents = useMemo(() => collectSubagents(items), [items]);
  // subagent 联动展开：toolId 集合——固定面板点击与对话卡受控展开双向同步
  const [expandedSubagents, setExpandedSubagents] = useState<ReadonlySet<string>>(new Set());
  const [input, setInput] = useState('');
  const [running, setRunning] = useState(false);
  const [disconnected, setDisconnected] = useState(false);
  // @补全引用：选中的文件路径 chip（发送时随 user_message 帧带 refs 字段）
  const [refs, setRefs] = useState<string[]>([]);
  // @ 弹层状态：start 为光标前最近 @ 的下标，query 为其后到光标的前缀
  const [mention, setMention] = useState<{ start: number; query: string } | null>(null);
  // 弹层高亮（受控）：父组件持 有 state，↑↓ 循环驱动，Enter/Tab 选中；MentionPopup
  // 通过 onFilesChange 上报可选中列表、列表变化时经 onActiveIdxChange 回卷首项
  const [mentionFiles, setMentionFiles] = useState<string[]>([]);
  const [mentionActiveIdx, setMentionActiveIdx] = useState(0);
  // 斜杠命令自动补全状态
  const [slashCommands, setSlashCommands] = useState<SlashCommand[]>([]);
  const [slashMention, setSlashMention] = useState<{ start: number; query: string } | null>(null);
  const [slashActiveIdx, setSlashActiveIdx] = useState(0);
  const [slashFilteredCommands, setSlashFilteredCommands] = useState<SlashCommand[]>([]);
  // ACP 会话配置快照（session_state/config_option_update 全量帧；空数组 = 非 ACP 或未就绪）
  const [configOptions, setConfigOptions] = useState<SessionConfigOption[]>([]);
  // Runner 路径审批模式（safe/auto_write/full_auto/plan）：初始值来自 prop，mode_updated 帧实时更新
  const [approvalMode, setApprovalMode] = useState(initialApprovalMode ?? 'safe');
  // 任务清单（todo_write 工具维护）：全量替换语义，todo_update 帧实时更新
  const [todos, setTodos] = useState<TodoItem[]>([]);
  // ACP 上下文用量快照（usage 帧实时更新；初始值从 sessions 缓存的
  // context_used/context_size 恢复——usage 已落库，刷新后用量条不丢）
  const [contextUsage, setContextUsage] = useState<{ used?: number; size?: number } | null>(null);
  // 上一回合耗时（ACP done 帧 duration_ms）；running 时隐藏，回合结束显示
  const [lastTurnDurationMs, setLastTurnDurationMs] = useState<number | null>(null);
  // config option 乐观更新的回滚快照：按 config_id 分键（prev=发送前值，opt=乐观值），
  // 并发点击不同选项互不覆盖（旧实现单槽快照互相覆盖，M19）。发送后保留，等
  // 服务端权威确认帧（session_state/config_option_update，已确认项移除）或「设置失败」
  // error 帧（回滚未确认项）。断线/重连时快照作废——它属于上一连接生命周期。
  const configRollbackRef = useRef<
    Record<string, { prev: string | boolean; opt: string | boolean }> | null
  >(null);
  // @ 角色补全候选：enabled 角色列表传给 MentionPopup，选中时替换 @query 为 @role-name
  const { data: rolesData } = useRoles({ enabled: true });
  const roles: AgentRole[] = useMemo(() => rolesData?.roles ?? [], [rolesData]);
  // 弹层点击外部关闭：textarea onBlur 延迟 150ms 关闭，让弹层项 click 先生效（onFocus 取消）
  const blurTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // IME 组词守卫：回车在组词中是「确认候选」而非发送（详见 useImeGuard）
  const ime = useImeGuard();
  const wsRef = useRef<WebSocket | null>(null);
  // 最近一帧到达时间（含应用层心跳）：看门狗据此判定连接假死（半开 TCP）。
  // 用组件级 ref——重连的 connect() 闭包都要读写它；effect 内局部变量会在 effect
  // 重建（语言切换等）时丢基线，新连接 onopen 虽重置，但旧连接存活期间重建会
  // 让看门狗误判。组件级 ref 在组件生命周期内恒定。
  const lastFrameAtRef = useRef(0);
  const bottomRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  // 消息区滚动容器 ref：虚拟化的 getScrollElement 目标。
  const scrollRef = useRef<HTMLDivElement>(null);
  // 「加载更早」按钮占位高度：虚拟化下按钮显隐会改变滚动内容高度，显隐切换时
  // 按此补偿 scrollTop，保持可视内容位置不因按钮占位变化而跳动（尤其 subagent
  // 固定面板定位）。按钮卸载后无法测高，故在 loadEarlier 期间（按钮仍挂载）捕获
  // 到 lastButtonHeightRef，effect 读它补偿。
  const earlierButtonRef = useRef<HTMLDivElement>(null);
  const lastButtonHeightRef = useRef(0);
  // 上次 hasMore 值：null = 首次装载（无滚动基线，不补偿）
  const prevHasMoreRef = useRef<boolean | null>(null);
  // 历史只在挂载时装载一次：refetch（done 后 invalidate）会改写聊天区，
  // 而对话中新增的 item 是会话内的实时增量，不能用服务器历史整体覆盖。
  const loadedRef = useRef(false);
  // 分页「加载更早」状态：has_more 表示是否还有更早消息；loading 为在飞请求。
  // loadedRawRef 保存所有已加载的原始消息行（rowid 升序），prepend 时把新页与
  // 它拼成完整集合做压缩重插去重（见 loadEarlier），保证跨页 summary 去重正确。
  const [hasMore, setHasMore] = useState(false);
  const [loadingEarlier, setLoadingEarlier] = useState(false);
  const loadingEarlierRef = useRef(false);
  const loadedRawRef = useRef<AgentMessage[]>([]);
  // 更早分页的原始行数（loadedRawRef 中「更早分页」与「最新页」的切分界标）。
  // 对账重载（断线重连/done 后 refetch）需要保留更早分页、只刷新最新页——按此前缀
  // 从 loadedRawRef 切出更早行，与 refetch 拿到的最新页拼成完整集合重建 items。
  const earlierCountRef = useRef(0);
  // items 的 ref 镜像：历史 effect 自愈守卫读（避免把 items 加入 effect 依赖）
  const itemsRef = useRef<ChatItem[]>([]);
  useEffect(() => {
    itemsRef.current = items;
  }, [items]);
  // 在飞工具调用（按 id 追踪），running 解除需其清空
  const pendingToolsRef = useRef<Set<string>>(new Set());
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // running 的 ref 镜像：WS onmessage 闭包内避免读旧 state
  const runningRef = useRef(false);
  // 当前正在流式写入的气泡 index（assistant_chunk 增量合并用；final/新事件到达时置 null）
  const streamingIdxRef = useRef<number | null>(null);
  // 流式气泡的种类：文本与 thought 分气泡（kind 切换即断流）
  const streamingKindRef = useRef<'assistant' | 'thought' | null>(null);
  // 流式 chunk 攒批缓冲（Map：key = chunkKey(parentToolId, kind)）。WS 帧先追加到
  // 这里，定时 flush 进 items（节流渲染）。主/子文本按 (parent, kind) 分键攒批，
  // 避免交错时互相串气泡——子 agent 文本收进父卡 children，不污染主流气泡。
  const chunkBufRef = useRef<Map<string, string>>(new Map());
  const chunkFlushTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // 子 agent 流式气泡定位：parentToolId → 该父卡 children 内当前流式气泡下标
  // （assistant/thought 分气泡）。父卡缺失或工具边界时删除，后续 chunk 新建气泡。
  const subStreamRef = useRef<Map<string, { idx: number; kind: 'assistant' | 'thought' }>>(
    new Map(),
  );
  // 用户是否接近底部（流式时仅接近底部才自动滚动，上翻读历史不被拽回）
  const stickToBottomRef = useRef(true);
  // 会话内最新 history 的 ref 镜像：done/重连后按状态决定是否需要重新装载
  // （React Query 后台 refetch 也会更新 history，不能仅凭引用变化就覆盖聊天区）
  const historyRef = useRef<typeof history>(undefined);
  // 本次装载是否为「半截装载」：history 末尾是 tool_calls/tool_result 行（回合在
  // 工具执行中被刷新/断线打断）。此时 DB 可能仍缺终态 flush 的文本/结果——done
  // 到达后允许 history refetch 重渲染完整历史（见 done 处理器），否则 loadedRef
  // 守卫会永远挡住对账。
  const partialLoadRef = useRef(false);
  // done 后对账重载的标记：history effect 读到它时跳过 running 兜底 heuristic
  // （对账重载的末行可能是 tool_result，按现状会误置 running=true——回合其实已终态）。
  const reconcileRef = useRef(false);
  // plan 气泡是否已在本回合出现过：回合内 plan 是「全量替换」语义（就地更新最后
  // 一条）；跨回合必须新建气泡——done/stopped/error/cancel_fallback/超时兜底时
  // 复位。否则新回合的 plan 会就地覆盖旧回合的 plan 位置，时间序颠倒（M17）。
  const planSeenThisTurnRef = useRef(false);
  // 已响应的审批/elicitation request_id 集合：防双击/连点重复提交（M18）。每
  // 个 request 只允许一次响应；组件重挂载自然重置（新会话从服务端重新对账）。
  const respondedRequestRef = useRef<Set<string>>(new Set());
  // 模型切换请求序号：仅最新一次切换的失败才回滚——旧请求失败回滚会覆盖后续
  // 已成功的切换值（并发切换竞态，M21）。
  const modelChangeSeqRef = useRef(0);

  const clearRunningTimeout = useCallback(() => {
    if (timeoutRef.current) {
      clearTimeout(timeoutRef.current);
      timeoutRef.current = null;
    }
  }, []);

  const stopRunning = useCallback(() => {
    runningRef.current = false;
    setRunning(false);
    clearRunningTimeout();
    pendingToolsRef.current.clear();
  }, [clearRunningTimeout]);

  // 回合终态处理：done/stopped/error/本地停止/10 分钟超时都把仍在 pending 的审批
  // 卡片置为 expired、pending 的 elicitation 卡片置为 cancelled。否则卡片永久
  // pending → hasPendingInteraction 恒 true → 发送按钮被锁死（服务端 5 分钟审批
  // 超时实际按 deny 继续回合、elicitation 超时按 Cancel 回 agent，UI 必须与服务端
  // 结果对齐）。expired 与用户主动 denied 区分：被动过期（超时/终态）vs 主动拒绝；
  // cancelled 同理区别于用户主动跳过（declined）。
  const expirePendingInteractions = useCallback(() => {
    setItems((prev) => prev.map((it) =>
      it.kind === 'approval' && it.approvalStatus === 'pending'
        ? { ...it, approvalStatus: 'expired' }
        : it.kind === 'elicitation' && it.elicitationStatus === 'pending'
          ? { ...it, elicitationStatus: 'cancelled' }
          : it
    ));
  }, []);

  // 不活动兜底触发：连续 RUNNING_TIMEOUT_MS 无任何回合帧（进程卡死/帧丢失）
  // 才判定超时——提示并强制解除 running（同时把 pending 审批置过期）。
  const fireRunningTimeout = useCallback(() => {
    timeoutRef.current = null;
    setItems((prev) => [...prev, { kind: 'system', systemTone: 'warning', content: tRef.current('agent.responseTimeout'), id: nextLiveItemId() }]);
    expirePendingInteractions();
    // 回合按终态处理：plan 归属随回合终结（下一回合首个 plan 新建气泡，M17）
    planSeenThisTurnRef.current = false;
    stopRunning();
  }, [expirePendingInteractions, stopRunning]);

  // 启动/重置 running 不活动兜底。关键：这是「静默超时」而非「回合总时长」——
  // 每收到一帧回合活动（onmessage 里调用）就重新倒计时。旧的绝对定时器从
  // 回合起算 10 分钟，ACP 长回合（长工具执行/多轮工具调用）跑到一半就被误报
  // 「响应超时」并过期仍在等待的审批卡，而回合其实还在正常流式推进。
  // 注：声明位置必须在历史装载 effect 之前（其 deps 数组引用本回调， TDZ）。
  const armRunningTimeout = useCallback(() => {
    clearRunningTimeout();
    timeoutRef.current = globalThis.setTimeout(fireRunningTimeout, RUNNING_TIMEOUT_MS);
  }, [clearRunningTimeout, fireRunningTimeout]);

  const armRunning = useCallback(() => {
    if (runningRef.current) return;
    runningRef.current = true;
    setRunning(true);
    armRunningTimeout();
  }, [armRunningTimeout]);

  // 消息区虚拟化：长会话只渲染视口内气泡，DOM 数量与 items 总数解耦（流式每
  // 50ms 更新时只 re-measure 视口内元素，避免长会话全量 DOM 布局卡顿）。
  // jsdom 无 ResizeObserver（measureElement 依赖它）时退化全量渲染，保证
  // 测试环境行为与既有实现一致。
  const canVirtualize = typeof ResizeObserver !== 'undefined';
  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 80,
    overscan: 8,
  });
  const virtualItems = canVirtualize ? virtualizer.getVirtualItems() : null;

  // subagent 展开翻转：固定面板点击与对话卡头部点击共用同一份受控状态
  const toggleExpandedSubagent = useCallback((toolId: string) => {
    setExpandedSubagents((prev) => {
      const next = new Set(prev);
      if (next.has(toolId)) next.delete(toolId);
      else next.add(toolId);
      return next;
    });
  }, []);

  // 固定面板行点击：联动展开对话中对应 subagent 卡 + 虚拟化滚动定位到该卡
  // （itemsRef 读最新 items，避免把 items 加入依赖导致回调重建）
  const handleSelectSubagent = useCallback(
    (index: number) => {
      const item = itemsRef.current[index];
      if (item?.toolId) {
        setExpandedSubagents((prev) => {
          const next = new Set(prev);
          if (next.has(item.toolId!)) next.delete(item.toolId!);
          else next.add(item.toolId!);
          return next;
        });
      }
      virtualizer.scrollToIndex(index, { align: 'center' });
    },
    [virtualizer],
  );

  // 按钮显隐补偿：hasMore 翻转（出现 true / 消失 false）时，滚动内容因按钮占位
  // 变化而整体位移 h——把 scrollTop 反向调整 h 使可视内容保持原位。首轮装载
  // （prevHasMoreRef 为 null）不补偿：挂载时 scrollTop=0 无基线，且首屏内容随
  // 历史装载自然下移属正常。h 由 loadEarlier 在按钮仍挂载时捕获（lastButtonHeightRef）。
  useEffect(() => {
    if (prevHasMoreRef.current === null) {
      prevHasMoreRef.current = hasMore;
      return;
    }
    if (prevHasMoreRef.current === hasMore) return;
    prevHasMoreRef.current = hasMore;
    const el = scrollRef.current;
    if (!el) return;
    const h = lastButtonHeightRef.current;
    if (h <= 0) return;
    // 按钮出现（false→true）：其占位把内容下推 h，需下滚 h 拉回；消失（true→false）反向
    el.scrollTop += hasMore ? h : -h;
  }, [hasMore]);

  // 历史消息（与 ActivityBar 的 Git 面板共享 queryKey，invalidate 后自动刷新）。
  // 关键：staleTime 0 + refetchOnMount 'always'。staleTime Infinity 会留下陈旧
  // 缓存——切到别的 session 再切回时 key={sessionId} 触发全新挂载，但 React
  // Query 直接命中旧缓存、不发请求，若离开期间回合已在服务端跑完落库，聊天区
  // 永远停留在旧内容。挂载时总是拉取，配合下面的「增量装载」保证不覆盖流式增量。
  const { data: history } = useQuery<AgentMessagesPage | AgentMessage[]>({
    queryKey: ['agent-messages', sessionId],
    queryFn: () => listAgentMessages(sessionId),
    refetchOnMount: 'always',
    refetchOnWindowFocus: false,
  });
  useEffect(() => {
    historyRef.current = history;
    const rows = historyRows(history);
    if (!history) return;
    // 自愈：已装载但聊天区为空而历史转非空（陈旧空缓存被 refetch 纠正）→ 允许重装
    if (loadedRef.current && !(itemsRef.current.length === 0 && rows.length > 0)) return;
    // done 后的对账重载（见 done 处理器）：只重建 items、跳过 running 兜底——
    // 该重载的末行可能是 tool_result（回合在工具执行中结束），按现状会误置
    // running=true 并锁死发送按钮 10 分钟，而回合其实已终态。
    const isReconcileReload = reconcileRef.current;
    reconcileRef.current = false;
    // 实时保护：挂载后 WS 流式增量先落地（history fetch 慢）、history 后到时，
    // 不应覆盖已渲染的实时消息——否则回合中一次无关重渲染（如展开卡片）就会
    // 清空聊天区。对账重载（done 后补全）显式放行覆盖。
    if (!isReconcileReload && itemsRef.current.length > 0) {
      loadedRef.current = true;
      return;
    }
    loadedRef.current = true;
    if (isReconcileReload) {
      // 对账重载（断线重连/done 后 refetch）：不再整体重置为最新一页——用户已加载
      // 的更早分页从视口消失要重新翻（DB 未丢但体验差）。改为合并：保留更早分页
      // 原始行（loadedRawRef 按 earlierCountRef 切分），只把「最新页」替换为
      // refetch 拿到的最新数据（断线期间服务端跑完落库/新增的消息在此补齐）。
      // hasMore 保持用户当前状态（不随首页重取复位，M20）。
      const earlierRows = loadedRawRef.current.slice(0, earlierCountRef.current);
      const mergedRaw = [...earlierRows, ...rows];
      loadedRawRef.current = mergedRaw;
      // 在完整合并集合上重算压缩重插去重：跨页重复/对账期间新增的 summary 重插段
      // 一并处理（groupByParent 在集合内跨页归组，父卡缺席的孤儿子项不会残留）。
      setItems(historyToChatItemsWithSkip(mergedRaw, compactionSkippedIndices(mergedRaw)));
      return;
    }
    if (rows.length > 0) {
      // 装载历史时若末尾是 tool_calls/tool_result 行，说明上次回合可能在工具执行中
      // 被打断（刷新/断线/服务端崩溃）。ACP 会话进程可能仍在跑（busy=true）。把
      // running 置 true 让用户看到「回合可能仍在执行」，直到 done/stopped/error 帧
      // 或 10 分钟超时解除。运行中发送已放开（服务端 busy 会排队），误置的代价只是
      // 指示器多亮一阵、消息走排队路径，优于用户以为回合已结束而重复发送。
      const last = rows[rows.length - 1];
      if ((last.kind === 'tool_calls' || last.kind === 'tool_result') && !runningRef.current) {
        // 半截装载标记：done 到达时允许 refetch 重渲染完整历史（对账）
        partialLoadRef.current = true;
        runningRef.current = true;
        setRunning(true);
        // 与 armRunning 同一不活动兜底：进程若仍在跑，后续活动帧会不断重置倒计时
        armRunningTimeout();
      }
    }
    // 纯函数装载：历史行 → ChatItem（含同一 tool_call_id 多行去重、压缩重插
    // 去重、plan 只留最后一条），见 history.ts。
    setItems(historyToChatItems(rows));
    loadedRawRef.current = rows;
    earlierCountRef.current = 0;
    // has_more 只在真正装载（重建 items）时随首页数据同步（M20）：done 对账重载、
    // 自愈重装都走这里。后台 refetch 被「实时保护」守卫早退、t 依赖重跑也早退，
    // 均不触达——否则用户翻页到底（hasMore=false）后一次 history refetch 就把
    // hasMore 复位成首页的 true，「加载更早」按钮闪烁/旧页消失。
    setHasMore(historyHasMore(history));
    // t 用于 running 超时提示文案；语言切换后重跑 effect 只影响尚未触发的
    // 超时回调文案，代价可忽略。armRunningTimeout 为稳定 useCallback，不引入额外重跑。
  }, [history, t, armRunningTimeout]);

  // 分页「加载更早消息」：以当前已加载最旧一条的 id 作 before 游标，取更早的一页，
  // 转换后 unshift 进 items 头部。不整体重建 items——流式渲染依赖 streamingIdxRef/
  // subStreamRef 等索引，整体替换会破坏进行中的流式气泡（尤其多标签页后台流式时）。
  // 去重只对「新页 + 已加载页」的完整集合做判断：压缩重插（kept 段原样复制）的原件
  // 若落在新页、重插副本在已加载页，跨页也能被 compactionSkippedIndices 命中并跳过
  //（把 skip 下标过滤到新页范围后传 historyToChatItemsWithSkip）。streamingIdxRef
  // 在同一个 setItems updater 里右移（flush 的 updater 按其入队顺序先读到旧下标，
  // 后读到已位移的下标，与 M1 的「ref 折进 updater 排队执行」同一语义）。
  const loadEarlier = async () => {
    if (loadingEarlierRef.current) return;
    const oldestId = loadedRawRef.current[0]?.id;
    if (!oldestId) return;
    loadingEarlierRef.current = true;
    setLoadingEarlier(true);
    // 按钮仍挂载时捕获其占位高度：本页加载完成后 hasMore 可能翻转（按钮卸载），
    // 显隐补偿 effect 需要此值来抵消滚动内容的高度变化（见 hasMore effect）。
    lastButtonHeightRef.current = earlierButtonRef.current?.offsetHeight ?? 0;
    try {
      const page = await listAgentMessages(sessionId, {
        before: oldestId,
        limit: EARLIER_PAGE_SIZE,
      });
      if (page.messages.length === 0) {
        setHasMore(false);
        return;
      }
      // 跨页压缩重插去重：完整集合（新页 + 已加载页）算 skip 后过滤到新页范围，
      // 详见 history.ts 的 prependSkip。
      const olderItems = historyToChatItemsWithSkip(
        page.messages,
        prependSkip(page.messages, loadedRawRef.current),
      );
      earlierCountRef.current += page.messages.length;
      loadedRawRef.current = [...page.messages, ...loadedRawRef.current];
      setHasMore(page.has_more);
      setItems((prev) => {
        // 跨页孤儿重归组：更早页的父 Task 卡到达时，把已加载页中指向它的顶层孤儿
        // 子项收进父卡 children（mergePages，见 subagent.ts）。返回被吸收孤儿在
        // prev 中的下标——孤儿从顶层移除后其后各项下标额外 -1，streamingIdxRef
        // 需据此修正（仅按 olderItems.length 位移会把流式气泡下标算偏，破坏续文合并）。
        const { items, absorbedIndexes } = mergePages(olderItems, prev);
        if (streamingIdxRef.current !== null) {
          let shift = olderItems.length;
          for (const i of absorbedIndexes) {
            if (i < streamingIdxRef.current) shift -= 1;
          }
          streamingIdxRef.current += shift;
        }
        return items;
      });
    } catch {
      // 加载失败静默：保留现状，用户可再次点击重试
    } finally {
      loadingEarlierRef.current = false;
      setLoadingEarlier(false);
    }
  };

  // running 的 ref 镜像供 onclose 闭包使用（onclose 里读不到最新 state）。
  // armRunning/stopRunning 是 useCallback，同步维护。
  // 把攒批的 chunk 缓冲一次性合并进流式气泡（新建或追加）。缓冲按
  // (parentToolId, kind) 分键：主 agent chunk 走主流气泡（streamingIdxRef），
  // 子 agent chunk 收进父卡 children 的流式气泡（subStreamRef）。同步置 ref
  // 保证与后续 setItems 更新的顺序一致（WS 回调在 React 外，flush 时机不依赖渲染）。
  const flushChunks = useCallback(() => {
    if (chunkFlushTimerRef.current) {
      clearTimeout(chunkFlushTimerRef.current);
      chunkFlushTimerRef.current = null;
    }
    if (chunkBufRef.current.size === 0) return;
    const buf = chunkBufRef.current;
    chunkBufRef.current = new Map();
    setItems((prev) => {
      let next = prev;
      for (const [key, content] of buf) {
        const { parent, kind } = parseChunkKey(key);
        if (!parent) {
          // 主 agent 流：并入当前流式气泡（同 kind），否则新建。气泡 kind 直接用
          // 攒批键里的 kind——旧实现读 streamingKindRef，但 flush 的 setItems
          // updater 延迟执行时其值可能已被后续 chunk 改写（读错 kind 会把 thought
          // 并入 assistant 气泡）。键内 kind 与 chunk 到达时的 kind 恒等。
          const idx = streamingIdxRef.current;
          if (idx !== null && next[idx]?.kind === kind) {
            next = next.map((it, i) =>
              i === idx ? { ...it, content: it.content + content } : it,
            );
          } else {
            streamingIdxRef.current = next.length;
            next = [...next, { id: nextLiveItemId(), kind, content }];
          }
          continue;
        }
        // 子 agent 流：收进父卡 children 的流式气泡
        const res = appendChildStream(next, parent, kind, content, subStreamRef.current.get(parent) ?? null);
        if (res.attached) {
          next = res.state;
          if (res.stream) subStreamRef.current.set(parent, res.stream);
        } else {
          // 父卡缺失（时序异常）：文本平铺进主流（带 parentToolId 标记）。父卡随后
          // 到达时经父卡创建的「孤儿收纳」移入 children；永不出现则保持平铺，内容不丢。
          next = [...next, { id: nextLiveItemId(), kind, content, parentToolId: parent }];
        }
      }
      return next;
    });
  }, []);

  /** 断开某个子 agent 的流式气泡（工具边界/终态）：后续该父卡的文本 chunk 新建气泡。
   *  与 breakStream 同语义（ref 置 null 折进 setItems updater 排队执行，先于其入队的
   *  flush updater 仍能读到当前气泡下标，M1）。 */
  const breakSubStream = useCallback((parentToolId: string) => {
    setItems((prev) => {
      subStreamRef.current.delete(parentToolId);
      return prev;
    });
  }, []);

  // 断开当前流式气泡（新事件类型到达/终态）：把 ref 置 null 折进 setItems updater
  // 排队执行。flushChunks 的 updater 惰性读 streamingIdxRef，若在此处同步置 null，
  // 会在 flush 的 updater 执行前把它清掉 → 工具/终态边界的尾文本恒新建碎片气泡
  // （M1）。改为在 updater 里置 null，保证先于其入队的 flush updater 读到的仍是
  // 当前气泡下标；updater 返回原引用，React bail out 不额外重渲染。
  const breakStream = useCallback(() => {
    setItems((prev) => {
      streamingIdxRef.current = null;
      streamingKindRef.current = null;
      return prev;
    });
  }, []);

  const scheduleChunkFlush = useCallback(() => {
    if (chunkFlushTimerRef.current) return;
    chunkFlushTimerRef.current = globalThis.setTimeout(() => {
      chunkFlushTimerRef.current = null;
      flushChunks();
    }, STREAM_FLUSH_MS);
  }, [flushChunks]);

  // 切换会话：清空上一会话的配置快照（新会话的 session_state 帧到达前不残留
  // 旧会话的 mode/effort 快捷按钮）
  useEffect(() => {
    setConfigOptions([]);
  }, [sessionId]);

  // WebSocket：断线自动重连（指数退避 1s→15s）。后端支持重连（新连接从 DB 重载
  // 会话，见 agent.rs handle_agent_socket），断线不应废掉整个会话的流式功能。
  useEffect(() => {
    let ws: WebSocket | null = null;
    let attempts = 0;
    let closedByCleanup = false;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    // 连接假死看门狗：中间设备静默掐断 TCP 时 onclose 不触发，浏览器重连逻辑
    // 依赖 onclose 永远不会执行。每 WATCHDOG_INTERVAL_MS 检查最近一帧，超过
    // HEARTBEAT_TIMEOUT_MS 无帧即主动 close——走既有 onclose 重连 + needHistoryReload
    // 历史对账路径（半开连接经服务端心跳探活确认已死，重连后按 DB 对齐）。
    let watchdogTimer: ReturnType<typeof setInterval> | null = null;
    // 断线且重连成功时需要重载历史：服务端在断线期间可能已把回合跑完并落库
    let needHistoryReload = false;
    // ref 在组件生命周期内恒定，复制到局部变量供 handler/cleanup 使用（exhaustive-deps）
    const pendingTools = pendingToolsRef.current;

    const connect = () => {
      ws = new WebSocket(agentWsUrl(sessionId));
      wsRef.current = ws;

      ws.onopen = () => {
        attempts = 0;
        setDisconnected(false);
        // 新连接给足一个完整心跳窗口：onopen 即重置看门狗基线（此后每帧刷新）
        lastFrameAtRef.current = Date.now();
        if (needHistoryReload) {
          needHistoryReload = false;
          // 允许历史 effect 重新装载（与断线期间服务端已落库的内容对齐）。
          // 必须置 reconcileRef：history effect 的「实时保护」守卫在聊天区非空时
          // 会早退并把 loadedRef 重新置 true，仅置 loadedRef=false 会被拦截——
          // 断线时聊天区几乎必然非空（历史 + 用户消息 + 连接中断提示），否则
          // 断线期间服务端跑完落库的内容永不补齐，需整页刷新。reconcileRef 与
          // done 对账重载同路径：放行覆盖并跳过 running 兜底 heuristic。
          reconcileRef.current = true;
          loadedRef.current = false;
          void queryClient.invalidateQueries({ queryKey: ['agent-messages', sessionId] });
        }
      };

      ws.onmessage = (ev) => {
      // 任意帧（含应用层心跳）到达都刷新看门狗基线：连接活着即不被误判假死
      lastFrameAtRef.current = Date.now();
      let msg: AgentWsEvent;
      try {
        msg = JSON.parse(ev.data) as AgentWsEvent;
      } catch {
        return;
      }
      // 回合活动重置不活动兜底：回合仍在推进时永不触发「响应超时」，只有连续
      // RUNNING_TIMEOUT_MS 无任何活动帧（进程卡死/帧丢失）才兜底解除 running。
      if (runningRef.current && TURN_ACTIVITY_TYPES.has(msg.type)) {
        armRunningTimeout();
      }
      if (msg.type === 'heartbeat') {
        // 应用层心跳：不渲染；连接活着的回合静默是合法的（长工具执行无输出），
        // 重置 running 不活动兜底——进程真卡死由服务端 idle reaper/exec 超时
        // 负责终态帧，前端不再对长任务误报「响应超时」。
        if (runningRef.current) armRunningTimeout();
      } else if (msg.type === 'assistant_chunk') {
        if (msg.content) {
          const parent = msg.parent_tool_call_id;
          const nextKind = msg.thought ? 'thought' : 'assistant';
          if (!parent) {
            // 主 agent 流：thought 与正文分气泡——kind 切换先 flush 当前缓冲，
            // 但不断流（下个 flush 的 `prev[idx].kind !== bubbleKind` 检查天然
            // 新建气泡，见 M1 注释）。
            if (streamingKindRef.current !== null && streamingKindRef.current !== nextKind) {
              flushChunks();
            }
            streamingKindRef.current = nextKind;
          }
          // 子 agent chunk 带 parent_tool_call_id：按 (parent, kind) 分键攒批，
          // 与主 agent 文本互不干扰（主/子交错时各自归键，flush 后各归其位）。
          const key = chunkKey(parent, nextKind);
          chunkBufRef.current.set(key, (chunkBufRef.current.get(key) ?? '') + msg.content);
          scheduleChunkFlush();
        }
        if (msg.final) {
          // 收尾：先冲掉缓冲里的增量（同帧 content+final 的非 SSE 回退也在此落齐），
          // 再关闭流式气泡（ref 置 null 走更新队列，与 flush 的 ref 写入保持顺序）。
          flushChunks();
          if (msg.parent_tool_call_id) breakSubStream(msg.parent_tool_call_id);
          else breakStream();
        }
      } else if (msg.type === 'stream_reset') {
        // 上游流传输失败重试：丢弃已缓冲的半截增量，并真正移除已 flush 实体化
        // 的半截气泡，让重试的完整文本从新气泡开始（后续 status 帧会提示重试次数）。
        const idx = streamingIdxRef.current;
        chunkBufRef.current = new Map();
        flushChunks(); // 清缓冲后为 no-op，仅取消 pending flush 定时器
        breakStream();
        setItems((prev) => {
          let next = prev;
          if (idx !== null) {
            const k = next[idx]?.kind;
            if (k === 'assistant' || k === 'thought') {
              next = next.filter((_, i) => i !== idx); // 真正移除半截气泡
            }
          }
          // 递归清理所有层级的 tool_call_chunk 流式占位卡（重试后流式从头开始）
          return dropStreamPlaceholders(next);
        });
      } else if (msg.type === 'tool_call') {
        const parentToolId = msg.parent_tool_call_id;
        const isSubagentCard = msg.is_subagent === true;
        // 子 agent 内部工具不 gate 主回合 running（父 Task 卡自身 tool_call 已
        // armRunning）；仅顶层工具（含父 Task 卡）追踪在飞 id
        if (!parentToolId) {
          if (msg.id) {
            pendingTools.add(msg.id);
          }
          armRunning();
        }
        // 工具回合与文本回合交替：先冲掉缓冲里的文本增量（保证气泡顺序）
        flushChunks();
        const toolItem: ChatItem = {
          kind: 'tool',
          content: '',
          toolId: msg.id,
          toolName: msg.name,
          parentToolId,
          toolArgs: msg.args,
          toolKind: msg.tool_kind,
          toolStatus: msg.status ?? 'in_progress',
          toolDiffs: msg.diffs,
          toolLocations: msg.locations,
          ...(isSubagentCard ? { isSubagent: true } : {}),
        };
        if (parentToolId) {
          // 子 agent 工具：断该父卡流式气泡（工具边界），再收纳子工具卡。
          // 父卡缺失（时序异常）→ 子卡平铺进主流（带 parentToolId 标记），父卡
          // 到达时经父卡创建的「孤儿收纳」移入 children；永不出现则保持平铺。
          breakSubStream(parentToolId);
          setItems((prev) => {
            const parentIdx = prev.findIndex(
              (it) => it.kind === 'tool' && it.toolId === parentToolId,
            );
            if (parentIdx < 0) {
              return upsertToolCard(prev, toolItem);
            }
            const next = [...prev];
            // 清理子 agent children 内残留的流式占位卡（__stream_ 合成键卡
            // 在正式 tool_call 帧到达后应被替换/清除）
            const cleanedChildren = dropStreamPlaceholders(next[parentIdx].children ?? []);
            next[parentIdx] = {
              ...next[parentIdx],
              children: upsertToolCard(cleanedChildren, toolItem),
            };
            return next;
          });
        } else {
          breakStream();
          setItems((prev) => {
            // 流式占位卡清理：tool_call_chunk 创建的 __stream_ 合成键占位卡
            // 在正式 tool_call 帧到达后被 upsertToolCard 按真实 id 替换，
            // 但合成键占位卡（无真实 id 匹配时）残留需清理。
            const cleaned = prev.filter(
              (it) => !(it.kind === 'tool' && it.toolId && it.toolId.startsWith(STREAM_TOOL_ID_PREFIX)),
            );
            // 去重：刷新/重连时 live tool_call 可能与 history 已渲染的卡片是同一
            // 工具（tool_call 已落库、tool_result 未到）。按 toolId 就地升级
            // （upsertToolCard：覆盖历史误判的 failed 状态、保留已收纳 children），
            // 而不是再追加一张重复卡——否则 tool_result 只 patch 一张，另一张永远
            // running。
            // 子 agent 父卡到达：先收集此前平铺在顶层的孤儿子项（parentToolId 命中
            // 本卡 toolId），从主流移除并作为 children 挂载（「孤儿收纳」），保证
            // 先到子事件最终收纳进父卡、不重复、不丢失。
            const orphanKids = toolItem.toolId
              ? cleaned.filter((it) => it.parentToolId === toolItem.toolId)
              : [];
            const filtered =
              orphanKids.length > 0
                ? cleaned.filter((it) => it.parentToolId !== toolItem.toolId)
                : cleaned;
            return upsertToolCard(filtered, {
              ...toolItem,
              ...(orphanKids.length > 0 ? { children: orphanKids } : {}),
            });
          });
        }
      } else if (msg.type === 'tool_call_chunk') {
        // runner 路径工具参数流式透出：占位卡就地更新（无 id 时用 index 合成键），
        // 正式 tool_call 帧到达后经 upsertToolCard 就地替换为完整卡片。
        armRunningTimeout(); // 重置不活动兜底（参数流式期间不算不活动）
        flushChunks();
        setItems((prev) => applyToolCallChunk(prev, msg));
      } else if (msg.type === 'tool_result') {
        const parentToolId = msg.parent_tool_call_id;
        if (parentToolId) {
          // 子 agent 工具结果：断该父卡流式气泡（结果边界），再 patch 子工具卡
          // （按 toolId 命中 children 内卡片就地更新）。父卡缺失 → 在顶层 patch
          // （子卡正平铺等待父卡；patchChildToolResult 未命中则追加结果卡）。
          flushChunks();
          breakSubStream(parentToolId);
          setItems((prev) => {
            const parentIdx = prev.findIndex(
              (it) => it.kind === 'tool' && it.toolId === parentToolId,
            );
            if (parentIdx < 0) {
              return patchChildToolResult(prev, msg);
            }
            const next = [...prev];
            next[parentIdx] = {
              ...next[parentIdx],
              children: patchChildToolResult(next[parentIdx].children ?? [], msg),
            };
            return next;
          });
        } else {
          if (msg.id) {
            pendingTools.delete(msg.id);
          }
          setItems((prev) => {
            const next = [...prev];
            const patch = (i: number) => {
              // args 覆盖（不用 ??）：claude-code-acp 的 ToolCall 首帧 rawInput 常是 {}
              // 占位，真正的命令/路径经 ToolCallUpdate.rawInput 由本帧携带——必须覆盖
              // 掉首帧的空 args，否则卡片头部摘要/展开详情永远停在空占位。
              // 防回归：本帧不带 args 时保留旧值（nullish 保留）。
              const isNoop = (a: string | undefined) => {
                const t = (a ?? '').trim();
                return t === '' || t === '{}';
              };
              next[i] = {
                ...next[i],
                toolResult: msg.result,
                toolStatus: msg.status ?? 'completed',
                toolName: next[i].toolName ?? msg.name,
                toolArgs: isNoop(next[i].toolArgs) && !isNoop(msg.args)
                  ? msg.args
                  : next[i].toolArgs ?? msg.args,
                toolKind: next[i].toolKind ?? msg.tool_kind,
                toolDiffs: next[i].toolDiffs ?? msg.diffs,
                toolLocations: next[i].toolLocations ?? msg.locations,
              };
            };
            // 权威匹配：按 toolId 精确命中（history 装载/live 追加的卡都带 id）。
            // 刷新后同名工具可能出现多次（或 tool_result 缺 name），id 是唯一可靠
            // 身份——比 name 扫描/「最早未完成」回退准确得多。
            if (msg.id) {
              const byId = next.findIndex((it) => it.kind === 'tool' && it.toolId === msg.id);
              if (byId >= 0) {
                patch(byId);
                return next;
              }
            }
            // 无 id 或 id 未命中：按 name 匹配（runner/旧帧语义）
            if (msg.name) {
              for (let i = next.length - 1; i >= 0; i--) {
                if (next[i].kind === 'tool' && next[i].toolName === msg.name && next[i].toolResult == null) {
                  patch(i);
                  return next;
                }
              }
            }
            // name 缺失/未命中：按 id 回退——遍历 items 取最早未完成卡片。ACP
            // 工具按序完成，最早未完成即当前工具；pendingTools 快照在重连后不可靠
            // （DB 已落库的卡无对应 pending），故直接在 items 上扫描。
            if (msg.id) {
              const pendingIdx: number[] = [];
              for (let i = 0; i < next.length; i++) {
                if (next[i].kind === 'tool' && next[i].toolResult == null) pendingIdx.push(i);
              }
              if (pendingIdx.length > 0) {
                patch(pendingIdx[0]);
              }
            }
            return next;
        });
        }
      } else if (msg.type === 'plan') {
        flushChunks();
        breakStream();
        const entries = msg.entries ?? [];
        // 本回合首个 plan：新建气泡（M17）。跨回合的 plan 不得就地覆盖旧回合的
        // plan 位置——否则新回合的计划盖在旧计划上，时间序颠倒。置位必须同步
        // （不能在 setItems updater 里写——updater 惰性执行，同一事件内连续两个
        // plan 帧时第二个仍读到 false，会误建第二个气泡）。
        if (!planSeenThisTurnRef.current) {
          planSeenThisTurnRef.current = true;
          setItems((prev) => [...prev, { kind: 'plan', content: '', planEntries: entries, id: nextLiveItemId() }]);
          return;
        }
        // 回合内后续 plan（ACP plan 全量替换语义）：就地更新最后一条；无则追加
        setItems((prev) => {
          for (let i = prev.length - 1; i >= 0; i--) {
            if (prev[i].kind === 'plan') {
              const next = [...prev];
              next[i] = { ...next[i], planEntries: entries };
              return next;
            }
          }
          return [...prev, { kind: 'plan', content: '', planEntries: entries, id: nextLiveItemId() }];
        });
      } else if (msg.type === 'usage') {
        // ACP 上下文用量快照：更新输入框上方的用量条（覆盖语义，取最新值）
        setContextUsage({ used: msg.used, size: msg.size });
      } else if (msg.type === 'attachment') {
        // ACP 多模态占位帧（image/audio/resource）：冲掉流式缓冲后追加附件占位卡
        flushChunks();
        breakStream();
        const parentId = msg.parent_tool_call_id;
        const card: ChatItem = {
          kind: 'attachment',
          content: '',
          id: nextLiveItemId(),
          attachmentKind: msg.media_kind ?? 'resource',
          attachmentName: msg.name ?? '',
          attachmentUri: msg.uri,
          attachmentMime: msg.mime,
          parentToolId: parentId,
        };
        if (parentId) {
          // 子 agent 产出：收进父 Task 卡 children；父卡缺失（时序异常）则平铺
          // 进主流（带 parentToolId 标记，父卡到达时经孤儿收纳移入 children）
          breakSubStream(parentId);
          setItems((prev) => {
            const parentIdx = prev.findIndex(
              (it) => it.kind === 'tool' && it.toolId === parentId,
            );
            if (parentIdx < 0) return [...prev, card];
            const next = [...prev];
            next[parentIdx] = {
              ...next[parentIdx],
              children: [...(next[parentIdx].children ?? []), card],
            };
            return next;
          });
        } else {
          setItems((prev) => [...prev, card]);
        }
      } else if (msg.type === 'status') {
        // 轻量提示行（压缩等中间状态）：复用 assistant 气泡样式但标记 status；
        // 不进气泡流 → 冲掉缓冲后断开流式气泡再追加独立行
        flushChunks();
        breakStream();
        setItems((prev) => [...prev, { kind: 'system', systemTone: 'info', content: msg.message ?? '', id: nextLiveItemId() }]);
      } else if (msg.type === 'queued') {
        // 运行中提交消息 → 服务端 busy 入队确认：轻量提示（不打断当前流式气泡）。
        // 队列在服务端（前端不做本地排队），本连接后续会收到该消息对应的流式帧。
        setItems((prev) => [...prev, { kind: 'system', systemTone: 'info', content: tRef.current('agent.messageQueued'), id: nextLiveItemId() }]);
      } else if (msg.type === 'stopped') {
        // 服务端确认取消（本连接或另一标签页发起的 cancel 都会广播到本连接的处理逻辑）
        flushChunks();
        breakStream();
        stopRunning();
        // 回合已终态：plan 归属随回合终结（下一回合首个 plan 新建气泡，M17），
        // 未响应的审批/elicitation 请求随回合作废 → 卡片置终态
        planSeenThisTurnRef.current = false;
        expirePendingInteractions();
      } else if (msg.type === 'cancel_fallback') {
        // 停止超时兜底：agent 进程未在时限内退出，服务端强制杀掉并重启。
        // 当前回合已死（上下文可能丢失）——按终态处理并提示用户。
        flushChunks();
        breakStream();
        stopRunning();
        planSeenThisTurnRef.current = false;
        expirePendingInteractions();
        setItems((prev) => [...prev, { kind: 'system', systemTone: 'warning', content: tRef.current('agent.cancelFallback'), id: nextLiveItemId() }]);
      } else if (msg.type === 'done') {
        // 终态：解除 Running。若在飞的工具帧随断线丢失，等回齐会把 UI 锁死
        // 10 分钟——done 到达即无条件解除（工具卡片增量渲染，无需等回齐）。
        flushChunks();
        breakStream();
        stopRunning();
        // 回合耗时（ACP 路径 done 帧携带；排队连续回合的中间帧无此字段）
        if (typeof msg.duration_ms === 'number') {
          setLastTurnDurationMs(msg.duration_ms);
        }
        // 递归清理所有层级的 tool_call_chunk 流式占位卡（安全网：正常流程中正式
        // tool_call 帧已替换占位卡，仅流中断时可能残留；递归清理含 children 内的）
        setItems((prev) => dropStreamPlaceholders(prev));
        // 回合成功结束：plan 归属随回合终结（下一回合首个 plan 新建气泡，M17）；
        // 服务端 5 分钟审批超时按 deny、elicitation 超时按 Cancel 继续回合，仍
        // pending 的卡片必须置终态，否则 hasPendingInteraction 恒 true 锁死发送按钮
        planSeenThisTurnRef.current = false;
        expirePendingInteractions();
        // 半截装载对账：本次会话是「刷新/断线时回合仍在跑」加载的，DB 当时缺
        // 终态 flush 的文本/结果（ACP 文本缓冲到终态落库）。done 到达时服务端
        // 已 flush 完整落库——重置 loadedRef 让紧随的 refetch 重渲染完整历史
        // （文本补全 + DB rowid 顺序），并置 reconcileRef 防 running heuristic 复发。
        if (partialLoadRef.current) {
          partialLoadRef.current = false;
          reconcileRef.current = true;
          loadedRef.current = false;
        }
        // 刷新共享的历史缓存，让 ActivityBar 的 Git 面板拿到最新 tool 结果；
        // 不影响聊天区（history effect 有 loadedRef 守卫，不会重复装载）
        void queryClient.invalidateQueries({ queryKey: ['agent-messages', sessionId] });
        // 回合成功结束 → 服务端异步生成会话标题，刷新会话列表让标题回显
        // （前缀匹配命中 SessionBar 的 ['agent-sessions', workspaceId]）
        void queryClient.invalidateQueries({ queryKey: ['agent-sessions'] });
      } else if (msg.type === 'session_title') {
        // 服务端标题已写库（生成晚于 done 帧，故此处单独广播）：刷新会话列表
        // 让 SessionBar 及时回显新标题
        void queryClient.invalidateQueries({ queryKey: ['agent-sessions'] });
      } else if (msg.type === 'session_state' || msg.type === 'config_option_update') {
        // 全量配置快照（session_state=初始，config_option_update=变更后）：归一化覆盖
        const serverOptions = normalizeConfigOptions(msg.options);
        setConfigOptions(serverOptions);
        // session_state 帧内嵌 available_commands（重连补发路径）：同步斜杠命令缓存
        if (msg.type === 'session_state' && Array.isArray(msg.available_commands)) {
          setSlashCommands(msg.available_commands);
        }
        // 服务端权威状态到达：确认生效的项从回滚快照移除（M19）——并发点击下旧
        // 实现直接清空整份快照，会让后点的选项（尚未确认）丢失回滚能力。
        reconcileConfigRollback(serverOptions);
      } else if (msg.type === 'current_mode_update') {
        // agent 侧自行切 mode（如 shift+tab）：同步 mode 项当前值
        setConfigOptions((prev) =>
          prev.map((o) =>
            o.category === 'mode' && msg.mode_id
              ? { ...o, currentValue: msg.mode_id }
              : o,
          ),
        );
      } else if (msg.type === 'mode_updated') {
        // Runner 路径审批模式切换确认：同步本地 plan/execute 状态
        if (msg.mode) setApprovalMode(msg.mode);
        // 同步 workspace 缓存（下一次 React Query refetch 前保持一致）
        void queryClient.invalidateQueries({ queryKey: ['agent-workspaces'] });
      } else if (msg.type === 'todo_update') {
        // 任务清单全量替换：模型维护进度面板
        setTodos(msg.todos ?? []);
      } else if (msg.type === 'available_commands') {
        // 斜杠命令全量快照（可能多次推送；空列表也要覆盖，agent 可清空命令）
        setSlashCommands(msg.commands ?? []);
      } else if (msg.type === 'approval_request') {
        // 危险操作审批：先冲掉缓冲里的文本增量，再追加审批卡片（等待用户响应）。
        // 有 options 时卡片渲染 agent 给的选项（ACP 透传），无则保持 approve/deny 二元。
        flushChunks();
        breakStream();
        setItems((prev) => [...prev, {
          kind: 'approval',
          content: '',
          approvalId: msg.request_id,
          approvalTool: msg.tool,
          approvalSummary: msg.summary,
          approvalOptions: msg.options,
          approvalStatus: 'pending',
          approvalArgsPreview: msg.args_preview,
        }]);
      } else if (msg.type === 'elicitation_request') {
        // ACP AskUserQuestion / MCP elicitation / refusal-fallback 表单：先冲掉缓冲
        // 里的文本增量，再追加表单卡片（等待用户填表，schema 由后端原始 JSON 透传）。
        flushChunks();
        breakStream();
        setItems((prev) => [...prev, {
          kind: 'elicitation',
          content: '',
          elicitationId: msg.request_id,
          elicitationMessage: msg.message,
          elicitationSchema: msg.schema,
          elicitationStatus: 'pending',
        }]);
      } else if (msg.type === 'error') {
        // 「设置失败」error 帧（服务端 set_config_option 失败，格式 `设置失败: {e}`）：
        // 乐观更新从未生效，回滚未确认项到发送前值（按选项快照，M19），按钮不再
        // 显示假性值。只回滚仍 pending 的选项，不波及已确认生效的项。
        if (configRollbackRef.current && msg.message?.startsWith('设置失败')) {
          const roll = configRollbackRef.current;
          configRollbackRef.current = null;
          setConfigOptions((cur) =>
            cur.map((o) =>
              roll[o.id] !== undefined ? restoreConfigValue(o, roll[o.id]!.prev) : o,
            ),
          );
        }
        flushChunks();
        breakStream();
        setItems((prev) => [...prev, { kind: 'system', systemTone: 'error', content: msg.message ?? '', id: nextLiveItemId() }]);
        stopRunning();
        // 回合以错误终态结束：plan 归属随回合终结（M17），未响应的审批/elicitation
        // 卡片一并置终态
        planSeenThisTurnRef.current = false;
        expirePendingInteractions();
      }
    };

      ws.onclose = () => {
        wsRef.current = null;
        // 回滚快照属于上一连接生命周期：断线即作废，重连后以 session_state 重新对齐
        configRollbackRef.current = null;
        if (closedByCleanup) return;
        // 断线：本地回合状态作废（服务端可能还在跑，也可能已丢），重连后按
        // DB 历史对齐。用户消息已发出去但服务端未必收到——提示而非静默重发。
        if (runningRef.current) {
          setItems((prev) => [
            ...prev,
            { kind: 'system', systemTone: 'warning', content: tRef.current('agent.connectionInterrupted'), id: nextLiveItemId() },
          ]);
        }
        stopRunning();
        // 断线时服务端 turn 被 drop、未响应审批按 deny、elicitation 按 Cancel 落定；
        // 本地卡片同样置终态，否则重连后历史 refetch 失败会永久锁死发送按钮
        expirePendingInteractions();
        setDisconnected(true);
        needHistoryReload = true;
        const delay = Math.min(1000 * 2 ** attempts, 15000);
        attempts++;
        reconnectTimer = globalThis.setTimeout(connect, delay);
      };
      ws.onerror = () => {
        // onerror 之后浏览器必发 onclose，统一在那里处理重连
      };
    };

    connect();
    watchdogTimer = globalThis.setInterval(() => {
      const w = wsRef.current;
      if (!w || w.readyState !== WebSocket.OPEN) return;
      if (Date.now() - lastFrameAtRef.current > HEARTBEAT_TIMEOUT_MS) {
        // 连接假死（半开 TCP 不触发 onclose）：主动 close 走既有 onclose 重连 +
        // needHistoryReload 历史对账路径
        w.close();
      }
    }, WATCHDOG_INTERVAL_MS);
    return () => {
      closedByCleanup = true;
      if (watchdogTimer) {
        clearInterval(watchdogTimer);
        watchdogTimer = null;
      }
      if (reconnectTimer) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      if (ws) {
        ws.onclose = null;
        ws.onerror = null;
        ws.onopen = null;
        ws.close();
      }
      wsRef.current = null;
      clearRunningTimeout();
      if (chunkFlushTimerRef.current) {
        clearTimeout(chunkFlushTimerRef.current);
        chunkFlushTimerRef.current = null;
      }
      chunkBufRef.current = new Map();
      pendingTools.clear();
    };
  }, [sessionId, queryClient, armRunning, armRunningTimeout, stopRunning, clearRunningTimeout, flushChunks, scheduleChunkFlush, expirePendingInteractions, breakStream, breakSubStream]);

  // 虚拟化下 getTotalSize() 随 measureElement 异步修正 item 高度而变：装载长会话时
  // 初始 estimate 不准，totalSize 稳定后需重新对齐底部，否则滚动停在半路、最新消息
  // 不可见。退化路径 totalSize 仅随 items 数量变化，行为与原先「items 变化即滚」一致。
  const totalSize = virtualizer.getTotalSize();
  useEffect(() => {
    // 仅当用户接近底部时才自动滚动（上翻读历史不被拽回）；直接滚动到底，
    // 避免逐 token smooth 动画互相堆积。jsdom 未实现 scrollIntoView，?.() 保底。
    if (stickToBottomRef.current) {
      bottomRef.current?.scrollIntoView?.({ behavior: 'auto' });
    }
  }, [items, totalSize]);

  // 多标签页模式：后台 tab 用 hidden 保持挂载（不卸载），尺寸/滚动位置不因切换
  // 而变。从隐藏变为可见（active false→true）且用户此前接近底部时，把视口重新
  // 对齐到最新消息（后台流式期间可能已新增内容），并让 virtualizer 重测尺寸。
  // 首次挂载即 active（第一个 tab）不触发——prevActiveRef 初始值即挂载时 active。
  const prevActiveRef = useRef(active);
  useEffect(() => {
    const wasInactive = prevActiveRef.current !== true;
    prevActiveRef.current = active;
    if (active && wasInactive && stickToBottomRef.current) {
      virtualizer.measure();
      bottomRef.current?.scrollIntoView?.({ behavior: 'auto' });
    }
  }, [active, virtualizer]);

  // 输入框自适应高度：内容驱动向上长高（输入框锚定底部悬浮），超 10 行才出滚动条
  const autoresizeInput = useCallback(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = 'auto';
    // jsdom 等无布局环境 scrollHeight 恒 0：保持 CSS 默认高度，不写内联样式
    if (el.scrollHeight === 0) return;
    const lineHeight = parseFloat(getComputedStyle(el).lineHeight) || 20;
    const max = lineHeight * 10 + 16; // 10 行 + 上下 padding
    el.style.height = `${Math.min(el.scrollHeight, max)}px`;
    el.style.overflowY = el.scrollHeight > max ? 'auto' : 'hidden';
  }, []);

  // 打字、发送后清空、@ 选中改写文本都汇聚到 input state，统一在此重算高度
  useEffect(() => {
    autoresizeInput();
  }, [input, autoresizeInput]);

  // 存在未响应的审批/elicitation 卡片时禁止继续发送（服务端在该响应前挂起回合）
  const hasPendingInteraction = items.some((it) =>
    (it.kind === 'approval' && it.approvalStatus === 'pending') ||
    (it.kind === 'elicitation' && it.elicitationStatus === 'pending'));

  // 审批响应：approved=true 时 remember 决定「仅本次」还是「本会话记住」；
  // ACP options 路径由 ApprovalCard 传入 optionId（原样回传 option_id，后端优先
  // 解析），remember 对 allow_always 选项置 true。仅帧真正发出后才落本地状态——
  // WS 断开时帧发不出去（服务端审批请求仍挂起），落本地「成功」会让用户误以为
  // 已响应、回合却永久卡住（M18）；此时提示连接丢失、卡片保持 pending。
  const respondApproval = (id: string, approved: boolean, remember: boolean, optionId?: string) => {
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      setItems((prev) => [...prev, { kind: 'system', systemTone: 'error', content: t('agent.connectionLost'), id: nextLiveItemId() }]);
      return;
    }
    // 防双击/连点：同一 request 只允许响应一次（M18）。二次点击帧不再发出，
    // 卡片状态保持首次点击结果。
    if (respondedRequestRef.current.has(id)) return;
    respondedRequestRef.current.add(id);
    const payload: Record<string, unknown> = {
      type: 'approval_response',
      request_id: id,
      approved,
      remember: remember ? 'session' : 'none',
    };
    if (optionId) {
      payload.option_id = optionId;
    }
    ws.send(JSON.stringify(payload));
    setItems((prev) => prev.map((it) =>
      it.kind === 'approval' && it.approvalId === id
        ? { ...it, approvalStatus: approved ? 'approved' : 'denied' }
        : it
    ));
  };

  // elicitation 响应：accept 带 content（仅非空时回传，服务端按 requested_schema
  // 解析字段值）；decline/cancel 无 content。仅帧真正发出后才落本地状态（卡片从
  // pending 变终态）——WS 断开时帧发不出去，落本地「成功」会让用户误以为已响应、
  // 服务端表单却一直挂着（M18）；此时提示连接丢失、卡片保持 pending。
  const respondElicitation = (
    id: string,
    action: 'accept' | 'decline' | 'cancel',
    content?: Record<string, unknown>,
  ) => {
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      setItems((prev) => [...prev, { kind: 'system', systemTone: 'error', content: t('agent.connectionLost'), id: nextLiveItemId() }]);
      return;
    }
    // 防双击/连点：同一 request 只允许响应一次（M18）。
    if (respondedRequestRef.current.has(id)) return;
    respondedRequestRef.current.add(id);
    const payload: Record<string, unknown> = {
      type: 'elicitation_response',
      request_id: id,
      action,
    };
    if (content && Object.keys(content).length > 0) payload.content = content;
    ws.send(JSON.stringify(payload));
    const status = action === 'accept' ? 'accepted' : action === 'decline' ? 'declined' : 'cancelled';
    setItems((prev) => prev.map((it) =>
      it.kind === 'elicitation' && it.elicitationId === id
        ? { ...it, elicitationStatus: status }
        : it
    ));
  };

  // @ 弹层触发检测：光标前找最近的 @（前面是空格/行首），其后到光标为 query。
  // 命中则打开弹层；query 含空白（@ 后直接空格）或光标前无 @ 则关闭。
  // / 斜杠命令：行首 / 开头的文本段（不含空白）触发命令补全浮层。
  const handleInputChange = (e: React.ChangeEvent<HTMLTextAreaElement>) => {
    const v = e.target.value;
    setInput(v);
    const pos = e.target.selectionStart ?? v.length;
    const before = v.slice(0, pos);
    // 优先检测 @ 提及（@ 可出现在任何位置）
    const at = before.lastIndexOf('@');
    if (at >= 0 && (at === 0 || /\s/.test(before[at - 1]))) {
      const q = before.slice(at + 1);
      if (!/\s/.test(q)) {
        closeSlashMention();
        setMention({ start: at, query: q });
        return;
      }
    }
    closeMention();
    // 行首 / 斜杠命令检测（仅当命令列表非空时启用）
    if (slashCommands.length > 0 && (before === '/' || (before.startsWith('/') && !before.slice(1).includes(' ')))) {
      setSlashMention({ start: 0, query: before.slice(1) });
      return;
    }
    closeSlashMention();
  };

  // 关闭 @ 弹层并清空受控高亮/列表状态：避免重开弹层时选中上一次的陈旧结果
  const closeMention = useCallback(() => {
    setMention(null);
    setMentionFiles([]);
    setMentionActiveIdx(0);
  }, []);

  // 关闭 / 斜杠命令弹层
  const closeSlashMention = useCallback(() => {
    setSlashMention(null);
    setSlashFilteredCommands([]);
    setSlashActiveIdx(0);
  }, []);

  // 选中 @ 提及项：角色（@name）→ 把 @query 替换为 @role-name 文本（服务端支持 @role-name
  // 前缀切换角色）；文件路径 → 把 @query 段移除，路径进 refs chip。
  const selectMention = (path: string) => {
    if (!mention) return;
    const before = input.slice(0, mention.start);
    const after = input.slice(mention.start + 1 + mention.query.length);
    if (path.startsWith('@')) {
      // 角色选择：替换为 @role-name 文本（带尾部空格便于继续输入）
      setInput(before + path + ' ' + after);
    } else {
      // 文件选择：移除 @query，路径进 refs chip
      setInput(before + after);
      setRefs((prev) => (prev.includes(path) ? prev : [...prev, path]));
    }
    closeMention();
    if (blurTimerRef.current) {
      clearTimeout(blurTimerRef.current);
      blurTimerRef.current = null;
    }
    textareaRef.current?.focus();
  };

  // 选中斜杠命令：把 /query 替换为 /command-name（尾部空格便于输入参数）
  const selectSlashCommand = (name: string) => {
    if (!slashMention) return;
    const before = input.slice(0, slashMention.start);
    const after = input.slice(slashMention.start + 1 + slashMention.query.length);
    setInput(before + '/' + name + ' ' + after);
    closeSlashMention();
    if (blurTimerRef.current) {
      clearTimeout(blurTimerRef.current);
      blurTimerRef.current = null;
    }
    textareaRef.current?.focus();
  };

  // 稳定回调（供 MentionPopup / SlashCommandPopup 的 effect 依赖）：setState 函数恒等，避免触发渲染循环
  const handleMentionFilesChange = useCallback((files: string[]) => {
    setMentionFiles(files);
  }, []);
  const handleMentionActiveIdxChange = useCallback((idx: number) => {
    setMentionActiveIdx(idx);
  }, []);
  const handleSlashCommandsChange = useCallback((cmds: SlashCommand[]) => {
    setSlashFilteredCommands(cmds);
  }, []);
  const handleSlashActiveIdxChange = useCallback((idx: number) => {
    setSlashActiveIdx(idx);
  }, []);

  const send = () => {
    const text = input.trim();
    // 运行中不再短路：服务端 busy 时会排队（回 queued 帧），消息不会丢。
    // hasPendingInteraction 仍需阻塞——服务端在该审批/elicitation 响应前挂起回合，
    // 发送必被吞。
    if (!text || hasPendingInteraction) return;
    const ws = wsRef.current;
    // WebSocket may be CONNECTING/CLOSED/CLOSING: sending throws InvalidStateError and
    // the message is silently lost, leaving running stuck true. Gate on OPEN instead.
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      setItems((prev) => [...prev, { kind: 'system', systemTone: 'error', content: t('agent.connectionLost'), id: nextLiveItemId() }]);
      return;
    }
    try {
      ws.send(JSON.stringify({ type: 'user_message', content: text, refs }));
    } catch {
      setItems((prev) => [...prev, { kind: 'system', systemTone: 'error', content: t('agent.connectionLost'), id: nextLiveItemId() }]);
      return;
    }
    setItems((prev) => [...prev, { kind: 'user', content: text, id: nextLiveItemId() }]);
    setInput('');
    setRefs([]);
    armRunning();
  };

  const stop = () => {
    const ws = wsRef.current;
    if (ws && ws.readyState === WebSocket.OPEN) {
      try {
        ws.send(JSON.stringify({ type: 'cancel' }));
      } catch {
        /* 发送失败也走本地停止 */
      }
    }
    // 先 flush 流式缓冲：停止时可能正有流式尾文本攒批未落屏，若不 flush，停止提示
    // 会先于尾文本出现（顺序颠倒）。breakStream 的 ref 置 null 走 setItems 队列，
    // 保证在 flush 的 updater 之后执行（M11/M1）。
    flushChunks();
    breakStream();
    stopRunning();
    // 本地停止路径同样作废未响应的审批/elicitation 卡片（cancel 帧可能因断线永远不回来）
    expirePendingInteractions();
    setItems((prev) => [...prev, { kind: 'system', systemTone: 'stopped', content: t('agent.stopped'), id: nextLiveItemId() }]);
  };

  const handleModelChange = (id: string) => {
    const prev = model;
    const seq = ++modelChangeSeqRef.current;
    onModelChange(id);
    void updateAgentSessionModel(sessionId, id)
      .then(() => {
        // 成功后 invalidate 会话列表缓存，让顶栏/会话列表的模型回显自愈
        void queryClient.invalidateQueries({ queryKey: ['agent-sessions'] });
      })
      .catch((err: unknown) => {
        // 失败：仅最新一次切换失败才回滚——旧请求失败回滚会覆盖后续已成功的切换
        // 值（并发切换竞态，M21）。陈旧请求既已让位，也不展示误导性失败提示。
        if (seq !== modelChangeSeqRef.current) return;
        onModelChange(prev);
        setItems((prevItems) => [
          ...prevItems,
          { kind: 'system', systemTone: 'error', content: `${t('agent.modelUpdateFailed')}: ${getApiErrorMessage(err)}`, id: nextLiveItemId() },
        ]);
      });
  };

  // ACP config option 切换：乐观更新 + WS 发送；发送失败或服务端「设置失败」
  // error 帧回滚（configRollbackRef 快照），生效确认以服务端回推的
  // config_option_update / session_state 全量帧为准。

  // 服务端权威配置帧到达后对账回滚快照：服务端值已等于乐观值的项 = 确认生效，
  // 从快照移除；仍 pending 的项保留（在途或失败待回滚）。并发点击不同选项时，
  // 后到确认帧不再清空整份快照，避免丢失其它在途选项的回滚能力（M19）。
  const reconcileConfigRollback = (serverOptions: SessionConfigOption[]) => {
    const roll = configRollbackRef.current;
    if (!roll) return;
    const next: typeof roll = {};
    for (const [id, entry] of Object.entries(roll)) {
      const serverVal = serverOptions.find((o) => o.id === id) && optionValue(serverOptions.find((o) => o.id === id)!);
      if (serverVal !== entry.opt) next[id] = entry;
    }
    configRollbackRef.current = Object.keys(next).length > 0 ? next : null;
  };

  const sendConfigOption = (configId: string, value: string) => {
    // 按选项记录回滚快照（M19）：prev=发送前值、opt=乐观值，按 config_id 分键，
    // 并发点击不同选项各自独立、互不覆盖；同选项重复点击保留首次 prev（true 前态）。
    // 快照不清空——发送成功与否要等服务端权威确认帧（reconcileConfigRollback 按
    // 确认移除）或「设置失败」error 帧（整体回滚未确认项）。
    const target = configOptions.find((o) => o.id === configId);
    if (target) {
      const optVal: string | boolean =
        target.type === 'boolean' ? value === 'true' : value;
      const roll = configRollbackRef.current ?? {};
      roll[configId] = { prev: roll[configId]?.prev ?? optionValue(target), opt: optVal };
      configRollbackRef.current = roll;
    }
    setConfigOptions((cur) =>
      cur.map((o) => {
        if (o.id !== configId) return o;
        if (o.type === 'boolean') {
          const b = value === 'true';
          return { ...o, currentBool: b, currentValue: b ? 'true' : 'false' };
        }
        return { ...o, currentValue: value };
      }),
    );
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      // 帧未发出：回滚本选项乐观值，快照一并作废
      const entry = configRollbackRef.current?.[configId];
      configRollbackRef.current = null;
      if (entry) {
        setConfigOptions((cur) =>
          cur.map((o) => (o.id === configId ? restoreConfigValue(o, entry.prev) : o)),
        );
      }
      return;
    }
    try {
      ws.send(JSON.stringify({ type: 'set_config_option', config_id: configId, value }));
    } catch {
      // send 同步抛错：帧未到达服务端，回滚本选项并作废快照
      const entry = configRollbackRef.current?.[configId];
      configRollbackRef.current = null;
      if (entry) {
        setConfigOptions((cur) =>
          cur.map((o) => (o.id === configId ? restoreConfigValue(o, entry.prev) : o)),
        );
      }
      setItems((prevItems) => [
        ...prevItems,
        { kind: 'system', systemTone: 'error', content: t('agent.connectionLost'), id: nextLiveItemId() },
      ]);
    }
  };

  // mode/effort 走右侧快捷按钮（发送按钮左边）；其余 options 进左侧统一菜单
  const modeOption = configOptions.find((o) => o.category === 'mode');
  const effortOption = configOptions.find((o) => o.category === 'thought_level');
  const menuOptions = configOptions.filter(
    (o) => o.category !== 'mode' && o.category !== 'thought_level',
  );

  // Runner 路径审批模式切换：发送 set_mode 帧（plan ↔ execute），乐观更新本地状态
  const sendSetMode = useCallback((newMode: string) => {
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    try {
      ws.send(JSON.stringify({ type: 'set_mode', mode: newMode }));
      setApprovalMode(newMode);
    } catch { /* ws closed */ }
  }, []);

  // 外部 prop 变化时同步本地 approvalMode（workspace 切换/页面刷新）
  useEffect(() => {
    if (initialApprovalMode) setApprovalMode(initialApprovalMode);
  }, [initialApprovalMode]);

  // 会话切换时从 sessions 缓存恢复用量快照（usage 已随帧落库；缓存未命中
  // 或快照为空时置 null，等首个 usage 帧到达再显示）
  useEffect(() => {
    const sessions = queryClient.getQueryData<AgentSession[]>(['agent-sessions', workspaceId]);
    const s = sessions?.find((x) => x.id === sessionId);
    setContextUsage(
      s && (s.context_used != null || s.context_size != null)
        ? { used: s.context_used ?? undefined, size: s.context_size ?? undefined }
        : null,
    );
    // 耗时为回合内瞬态展示，切会话即失效
    setLastTurnDurationMs(null);
  }, [queryClient, workspaceId, sessionId]);

  // 单条消息渲染：虚拟化与全量路径共用。streaming 标记当前正在流式写入的气泡
  // （assistant/thought），MessageBubble 据此用 `<Markdown streaming />` 渲染
  // （保留 md 结构、去掉 code 插件避免 Shiki 每帧全量重高亮，见 Markdown.tsx）。
  // 子 agent 父卡（isSubagent 或
  // 带 children 的 tool 卡——历史路径只落 parent_tool_call_id、无 is_subagent 标记，
  // 按 children 有无推断）走 SubagentTaskCard 嵌套渲染 children。
  const renderItem = (it: ChatItem, i: number) => {
    const isStreaming =
      streamingIdxRef.current === i && (it.kind === 'assistant' || it.kind === 'thought');
    if (it.kind === 'system') {
      // 稳定 key：stream_reset 移除流式气泡后其余项下标位移，index key 会让
      // 后续气泡整体重挂载（丢内部展开态/触发 Shiki 重复高亮），改用 id。
      return <SystemMessage key={it.id ?? i} tone={it.systemTone} content={it.content} />;
    }
    if (it.kind === 'approval') {
      return <ApprovalCard key={it.approvalId ?? it.id ?? i} item={it} onRespond={respondApproval} />;
    }
    if (it.kind === 'elicitation') {
      return <ElicitationCard key={it.elicitationId ?? it.id ?? i} item={it} onRespond={respondElicitation} />;
    }
    if (it.kind === 'tool' && (it.isSubagent || (it.children && it.children.length > 0))) {
      return (
        <SubagentTaskCard
          key={it.toolId ?? it.id ?? i}
          item={it}
          streamingChildIdx={it.toolId ? subStreamRef.current.get(it.toolId)?.idx : undefined}
          // 受控展开：与 subagent 固定面板联动（toolId 缺失时保持非受控内部态）
          open={it.toolId ? expandedSubagents.has(it.toolId) : undefined}
          onToggle={it.toolId ? () => toggleExpandedSubagent(it.toolId!) : undefined}
        />
      );
    }
    return (
      <MessageBubble
        // tool 卡保持 toolId 优先（live 卡与历史卡跨对账重载共用同一 key，展开态不丢）；
        // 其余气泡用稳定 id（stream_reset 移除流式气泡后下标位移也不重挂载）。
        key={it.kind === 'tool' && it.toolId ? it.toolId : (it.id ?? i)}
        item={it}
        streaming={isStreaming}
      />
    );
  };

  return (
    <div className="relative flex h-full">
      {/* 对话列（消息滚动 + 悬浮输入框）；移动端在顶部插入 subagent 固定面板，
          桌面端由右侧栏承担（见下方 sidebar 分支） */}
      <div className="relative flex min-w-0 flex-1 flex-col">
        {!isDesktop && subagents.length > 0 && (
          <SubagentPanel
            variant="top"
            summaries={subagents}
            onSelect={handleSelectSubagent}
            expandedIds={expandedSubagents}
          />
        )}
      <div
        ref={scrollRef}
        data-testid="chat-scroll-container"
        className="flex-1 overflow-y-auto px-3 pt-3 md:px-5 md:pt-4 dark:text-foreground/85"
        onScroll={(e) => {
          const el = e.currentTarget;
          // 距底 < 80px 视为「跟随流式输出」；上翻超过阈值即停止自动滚动
          stickToBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
        }}
      >
        {/* 限宽包裹层：与悬浮输入框共用 max-w-3xl，在滚动容器 content 内同一基准
            居中——滚动条出现时 content 同步缩窄，sticky 输入框作为本层最后一个
            子元素自动跟随，因此有/无滚动条时消息流与输入框左/右边缘都精确对齐。
            min-h-full + flex-col 保证消息不足一屏时输入框仍沉底。 */}
        <div className="mx-auto flex w-full min-h-full max-w-3xl flex-col">
        <div className="flex-1">
        {hasMore && items.length > 0 && (
          <div ref={earlierButtonRef} className="flex justify-center py-1.5">
            <Button
              type="button"
              variant="ghost"
              size="sm"
              onClick={() => void loadEarlier()}
              disabled={loadingEarlier}
              className="h-7 px-3 text-xs text-muted-foreground hover:text-foreground"
            >
              {loadingEarlier ? t('agent.loadingEarlier') : t('agent.loadEarlierMessages')}
            </Button>
          </div>
        )}
        {items.length === 0 && !running && (
          <p className="text-center text-sm text-muted-foreground">{t('agent.chatEmptyHint')}</p>
        )}
        {virtualItems ? (
          // 虚拟化路径：只渲染视口附近的气泡。item div 用 absolute + translateY
          // 定位（totalSize 撑起滚动高度），间距用 padding 而非 margin——measureElement
          // 只测 border-box，margin 不计入会导致相邻项重叠。pb-3/md:pb-4 对齐原 space-y。
          <div style={{ height: virtualizer.getTotalSize(), position: 'relative' }}>
            {virtualItems.map((vi) => (
              <div
                key={vi.index}
                ref={virtualizer.measureElement}
                data-index={vi.index}
                className="pb-3 md:pb-4"
                style={{
                  position: 'absolute',
                  top: 0,
                  left: 0,
                  width: '100%',
                  transform: `translateY(${vi.start}px)`,
                }}
              >
                {renderItem(items[vi.index], vi.index)}
              </div>
            ))}
          </div>
        ) : (
          // 退化路径（jsdom/无 ResizeObserver）：全量渲染，保持原 space-y 布局
          <div className="space-y-3 md:space-y-4">
            {items.map((it, i) => renderItem(it, i))}
          </div>
        )}
        <div ref={bottomRef} />
        </div>

        {/* 悬浮输入框：sticky 钉在滚动容器可视底部，作为限宽层最后一个子元素与
            消息流共享同一 content 宽度基准（滚动条占位同步收缩），因此有/无滚动条
            时都与消息流左/右边缘精确对齐。sticky 在文档流中占位，滚动到底时输入框
            落在消息流末尾；-mx-3 md:-mx-5 把背景横向铺满滚动容器 padding 区，
            滚动内容从输入框底下经过时被 bg-card 遮挡。顶部 absolute 渐隐让内容
            淡出到输入框，不占文档流高度。 */}
        <div className="sticky bottom-0 z-20 -mx-3 bg-card px-3 pb-[max(env(safe-area-inset-bottom),var(--sat-bottom,0px),0.75rem)] pt-1.5 md:-mx-5 md:px-5 md:pb-5 md:pt-2">
          <div className="pointer-events-none absolute inset-x-0 bottom-full h-9 bg-gradient-to-t from-card to-transparent" />
          <div className="mx-auto w-full max-w-3xl">
          {running && (
            /* 运行中指示：视觉已迁移到输入框彩色边框（容器 .agent-input-running，
               见 index.css），此处仅保留视觉隐藏的 status 语义供屏幕阅读器播报。
               注意：不能放在消息流内——absolute 定位的 sr-only 会触发滚动容器
               sticky 输入框跳顶的 Chrome 布局 bug（见 05895ca 回归），故置于
               sticky 输入框容器内。 */
            <span role="status" aria-label={t('agent.running')} className="sr-only" />
          )}
          {disconnected && (
            <div className="mb-1 flex items-center gap-1.5 rounded-md bg-destructive/10 px-2.5 py-1.5 text-xs text-destructive md:mb-1.5">
              <Loader2 className="h-3 w-3 animate-spin" />
              {t('agent.reconnecting')}
            </div>
          )}
          {/* 任务清单面板：todo_write 工具维护，全量替换语义 */}
          {todos.length > 0 && (
            <div className="mb-2 rounded-xl border border-border/60 bg-muted/30 px-3 py-2">
              <div className="mb-1 text-xs font-medium text-muted-foreground">Tasks</div>
              <ul className="space-y-0.5">
                {todos.map((t, i) => (
                  <li key={i} className="flex items-start gap-1.5 text-xs">
                    <span className="mt-0.5 shrink-0">
                      {t.status === 'completed' ? '✅' : t.status === 'in_progress' ? '🔄' : '⬜'}
                    </span>
                    <span className={t.status === 'completed' ? 'text-muted-foreground line-through' : t.status === 'in_progress' ? 'font-medium' : ''}>
                      {t.activeForm && t.status === 'in_progress' ? `${t.activeForm}: ` : ''}{t.content}
                    </span>
                  </li>
                ))}
              </ul>
            </div>
          )}
          {/* ACP 上下文用量条：usage 帧/会话快照驱动；>80% 黄、>95% 红 */}
          {contextUsage?.size != null && contextUsage.size > 0 && (
            (() => {
              const used = contextUsage.used ?? 0;
              const size = contextUsage.size;
              const pct = Math.min(100, Math.round((used / size) * 100));
              const tone =
                pct > 95 ? 'bg-destructive' : pct > 80 ? 'bg-yellow-500' : 'bg-primary/60';
              const fmt = (n: number) =>
                n >= 1000 ? `${(n / 1000).toFixed(n >= 100_000 ? 0 : 1)}k` : String(n);
              return (
                <div
                  className="mb-2 flex items-center gap-2 px-1"
                  data-testid="context-usage-bar"
                  title={t('agent.contextUsageTooltip', { used, size })}
                >
                  <div className="h-1 flex-1 overflow-hidden rounded-full bg-muted">
                    <div className={`h-full rounded-full transition-all ${tone}`} style={{ width: `${pct}%` }} />
                  </div>
                  <span className="shrink-0 text-[10px] tabular-nums text-muted-foreground">
                    {fmt(used)}/{fmt(size)} · {pct}%
                  </span>
                </div>
              );
            })()
          )}
          {/* 上一回合耗时（ACP done 帧 duration_ms；running 时隐藏，切会话清除） */}
          {lastTurnDurationMs != null && !running && (
            <div
              className="mb-2 px-1 text-[10px] tabular-nums text-muted-foreground"
              data-testid="turn-duration"
            >
              {t('agent.turnDuration')}{' '}
              {lastTurnDurationMs < 1000
                ? `${lastTurnDurationMs}ms`
                : `${(lastTurnDurationMs / 1000).toFixed(1)}s`}
            </div>
          )}
          {/* 运行时输入框边框换成彩色渐变流动（.agent-input-running），空闲恢复默认描边 */}
          <div className={`relative rounded-2xl border bg-background shadow-2xl focus-within:ring-1 focus-within:ring-ring ${running ? 'agent-input-running' : 'border-input'}`}>
          {refs.length > 0 && (
            <div className="flex flex-wrap gap-1 px-2 pt-1.5">
              {refs.map((r) => (
                <span key={r} className="inline-flex items-center gap-1 rounded-md bg-primary/10 px-2 py-0.5 text-xs text-primary">
                  @{r}
                  <button type="button" onClick={() => setRefs((prev) => prev.filter((x) => x !== r))} className="hover:text-destructive">×</button>
                </span>
              ))}
            </div>
          )}
          {mention && (
            <MentionPopup
              workspaceId={workspaceId}
              query={mention.query}
              activeIdx={mentionActiveIdx}
              onActiveIdxChange={handleMentionActiveIdxChange}
              onFilesChange={handleMentionFilesChange}
              onSelect={selectMention}
              roles={roles}
            />
          )}
          {slashMention && slashCommands.length > 0 && (
            <SlashCommandPopup
              commands={slashCommands}
              query={slashMention.query}
              activeIdx={slashActiveIdx}
              onActiveIdxChange={handleSlashActiveIdxChange}
              onCommandsChange={handleSlashCommandsChange}
              onSelect={selectSlashCommand}
            />
          )}
          {/* iOS 上 <16px 的输入框聚焦会触发自动页面缩放：移动端用 16px（text-base）、桌面保持 14px */}
          <textarea
            ref={textareaRef}
            value={input}
            onChange={handleInputChange}
            {...ime.bind}
            onKeyDown={(e) => {
              // IME 组词中（拼音候选窗）的按键不触发任何快捷键：回车是确认候选而非发送
              if (ime.isComposing(e)) return;
              if (e.key === 'Escape') {
                closeMention();
                closeSlashMention();
                return;
              }
              if (mention) {
                // @ 弹层打开时键盘操作：↑↓ 循环移动高亮、Enter/Tab 选中、Shift+Enter 放行换行
                if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
                  e.preventDefault();
                  const n = mentionFiles.length;
                  if (n > 0) {
                    setMentionActiveIdx((prev) =>
                      e.key === 'ArrowDown' ? (prev + 1) % n : (prev - 1 + n) % n,
                    );
                  }
                  return;
                }
                if (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey)) {
                  e.preventDefault();
                  const target = mentionFiles[mentionActiveIdx];
                  if (target) selectMention(target);
                  return;
                }
                // Shift+Enter 或其它键：不拦截，交给下方 Enter/默认行为
              } else if (slashMention) {
                // / 斜杠命令弹层键盘操作
                if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
                  e.preventDefault();
                  const n = slashFilteredCommands.length;
                  if (n > 0) {
                    setSlashActiveIdx((prev) =>
                      e.key === 'ArrowDown' ? (prev + 1) % n : (prev - 1 + n) % n,
                    );
                  }
                  return;
                }
                if (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey)) {
                  e.preventDefault();
                  const target = slashFilteredCommands[slashActiveIdx];
                  if (target) selectSlashCommand(target.name);
                  return;
                }
              }
              if (e.key === 'Enter' && !e.shiftKey) {
                e.preventDefault();
                send();
              }
            }}
            onBlur={() => {
              // 点击弹层项会先触发 textarea blur：延迟 150ms 关闭，让 click 先选中
              if (mention) {
                blurTimerRef.current = globalThis.setTimeout(closeMention, 150);
              }
              if (slashMention) {
                blurTimerRef.current = globalThis.setTimeout(closeSlashMention, 150);
              }
            }}
            onFocus={() => {
              // 用户回到输入框（或弹层项选中后主动 focus）→ 取消待执行的关闭
              if (blurTimerRef.current) {
                clearTimeout(blurTimerRef.current);
                blurTimerRef.current = null;
              }
            }}
            placeholder={t('agent.inputPlaceholder')}
            className="w-full min-h-[2.25rem] resize-none rounded-t-2xl border-0 bg-transparent px-3 pb-1 pt-2 text-base leading-5 focus:outline-none md:text-sm"
            rows={1}
          />
          {/* 底部操作行：上边框与输入区分隔（模型/模式/effort 按钮 vs 文本输入） */}
          <div className="flex flex-wrap items-center justify-between gap-1 border-t border-border/60 px-1.5 pb-1.5 pt-1 md:px-2">
            <SessionSettingsMenu
              model={model}
              onModelChange={handleModelChange}
              configOptions={menuOptions}
              onConfigChange={sendConfigOption}
            />
            <div className="flex items-center gap-0.5">
              {/* Plan 模式切换按钮（runner 路径）：ACP 会话（已上报 config_options）隐藏——
                  ACP 的 mode 切换走右侧 ConfigOptionButton（set_config_option 帧），
                  set_mode 帧只改 workspace.approval_mode（runner 审批语义），ACP 下无意义。 */}
              {configOptions.length === 0 && (
                <>
                  {approvalMode === 'plan' && (
                    <span className="inline-flex items-center gap-1 rounded-full bg-blue-100 px-2 py-0.5 text-xs font-medium text-blue-700 dark:bg-blue-900/30 dark:text-blue-300">
                      {t('agent.approvalMode_plan')}
                    </span>
                  )}
                  <Button
                    size="sm"
                    variant={approvalMode === 'plan' ? 'default' : 'ghost'}
                    className="h-7 rounded-full px-2 text-xs"
                    onClick={() => sendSetMode(approvalMode === 'plan' ? 'safe' : 'plan')}
                    title={approvalMode === 'plan' ? t('agent.approvalModeHint_plan') : t('agent.approvalModeHint_safe')}
                  >
                    {approvalMode === 'plan' ? t('agent.approvalMode_plan') : 'Plan'}
                  </Button>
                </>
              )}
              <ConfigOptionButton
                option={modeOption}
                label="agent.configMode"
                onChange={sendConfigOption}
                placeholder={configOptions.length > 0 && !modeOption}
              />
              <ConfigOptionButton
                option={effortOption}
                label="agent.configEffort"
                onChange={sendConfigOption}
                placeholder={configOptions.length > 0 && !effortOption}
              />
              {/* 发送/暂停按输入动态切换（Claude Code 风格）：对话进行中若输入框
                  有文字则显示发送（服务端 busy 排队），无文字则显示停止；空闲时
                  固定显示发送。二者不并存。 */}
              {running && !input.trim() ? (
                <Button
                  onClick={stop}
                  size="sm"
                  variant="ghost"
                  aria-label={t('agent.stop')}
                  className="h-8 w-8 rounded-full p-0 text-destructive hover:text-destructive"
                >
                  <Square className="h-4 w-4 fill-current" />
                </Button>
              ) : (
                <Button
                  onClick={send}
                  disabled={!input.trim() || hasPendingInteraction}
                  size="sm"
                  variant="ghost"
                  aria-label={t('agent.send')}
                  className="h-8 w-8 rounded-full p-0"
                >
                  <SendHorizontal className="h-4 w-4" />
                </Button>
              )}
            </div>
          </div>
          </div>
        </div>
        </div>
        </div>
      </div>
      </div>
      {/* 桌面端：右侧固定 subagent 栏（占文档流全高，不覆盖对话滚动条） */}
      {isDesktop && subagents.length > 0 && (
        <SubagentPanel
          variant="sidebar"
          summaries={subagents}
          onSelect={handleSelectSubagent}
          expandedIds={expandedSubagents}
        />
      )}
    </div>
  );
}
