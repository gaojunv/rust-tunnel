import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Button } from '@/components/ui/button';
import { Loader2, SendHorizontal, Square } from 'lucide-react';
import {
  agentWsUrl,
  getApiErrorMessage,
  listAgentMessages,
  updateAgentSessionModel,
} from '../../api/client';
import type { AgentWsEvent } from '../../types';
import { parseAcpToolJson, parsePlanEntries } from './types';
import type { ChatItem, ToolDiff, ToolKind, ToolLocation } from './types';
import ApprovalCard from './ApprovalCard';
import MentionPopup from './MentionPopup';
import MessageBubble from './MessageBubble';
import SessionSettingsMenu from './SessionSettingsMenu';
import SystemMessage from './SystemMessage';
import ConfigOptionButton from './ConfigOptionButton';
import { normalizeConfigOptions } from './sessionConfig';
import type { SessionConfigOption } from '../../types';

const RUNNING_TIMEOUT_MS = 10 * 60 * 1000; // 10 分钟兜底
/** 流式 chunk 合并 flush 间隔：token 级 WS 帧攒批后一次性写 state，避免每 token 全列表重渲染。 */
export const STREAM_FLUSH_MS = 50;

interface Props {
  sessionId: string;
  workspaceId: string;
  model: string;
  onModelChange: (id: string) => void;
}

export default function ChatStream({ sessionId, workspaceId, model, onModelChange }: Props) {
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
  // ACP 会话配置快照（session_state/config_option_update 全量帧；空数组 = 非 ACP 或未就绪）
  const [configOptions, setConfigOptions] = useState<SessionConfigOption[]>([]);
  // config option 乐观更新的回滚快照：发送后保留，等服务端权威确认帧
  // （session_state/config_option_update，确认生效则清空）或「设置失败」error 帧
  // （回滚到快照）。断线/重连时快照作废——它属于上一连接生命周期。
  const configRollbackRef = useRef<SessionConfigOption[] | null>(null);
  // 弹层点击外部关闭：textarea onBlur 延迟 150ms 关闭，让弹层项 click 先生效（onFocus 取消）
  const blurTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const wsRef = useRef<WebSocket | null>(null);
  const bottomRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  // 悬浮输入框高度：消息区底部留出同高占位，保证末尾消息可滚动到输入框之上
  const [inputFloatH, setInputFloatH] = useState(0);
  const inputCardRef = useRef<HTMLDivElement>(null);
  // 历史只在挂载时装载一次：refetch（done 后 invalidate）会改写聊天区，
  // 而对话中新增的 item 是会话内的实时增量，不能用服务器历史整体覆盖。
  const loadedRef = useRef(false);
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
  // 在飞 tool_call id 顺序表：tool_result 缺 name 时按 id 回退匹配卡片
  const toolIdsRef = useRef<string[]>([]);
  // 流式 chunk 攒批缓冲：WS 帧先追加到这里，定时 flush 进 items（节流渲染）
  const chunkBufRef = useRef('');
  const chunkFlushTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // 用户是否接近底部（流式时仅接近底部才自动滚动，上翻读历史不被拽回）
  const stickToBottomRef = useRef(true);
  // 会话内最新 history 的 ref 镜像：done/重连后按状态决定是否需要重新装载
  // （React Query 后台 refetch 也会更新 history，不能仅凭引用变化就覆盖聊天区）
  const historyRef = useRef<typeof history>(undefined);

  // 历史消息（与 ActivityBar 的 Git 面板共享 queryKey，invalidate 后自动刷新）。
  // 关键：staleTime 0 + refetchOnMount 'always'。staleTime Infinity 会留下陈旧
  // 缓存——切到别的 session 再切回时 key={sessionId} 触发全新挂载，但 React
  // Query 直接命中旧缓存、不发请求，若离开期间回合已在服务端跑完落库，聊天区
  // 永远停留在旧内容。挂载时总是拉取，配合下面的「增量装载」保证不覆盖流式增量。
  const { data: history } = useQuery({
    queryKey: ['agent-messages', sessionId],
    queryFn: () => listAgentMessages(sessionId),
    refetchOnMount: 'always',
    refetchOnWindowFocus: false,
  });
  useEffect(() => {
    historyRef.current = history;
    if (!history) return;
    // 自愈：已装载但聊天区为空而历史转非空（陈旧空缓存被 refetch 纠正）→ 允许重装
    if (loadedRef.current && !(itemsRef.current.length === 0 && history.length > 0)) return;
    loadedRef.current = true;
    // 装载历史时若末尾是 tool_calls/tool_result 行，说明上次回合可能在工具执行中
    // 被打断（刷新/断线/服务端崩溃）。ACP 会话进程可能仍在跑（busy=true），此时
    // 发送会撞 "ACP 回合进行中" busy 守卫、用户消息被静默吞掉。把 running 置 true
    // 让用户看到「回合可能仍在执行」，直到 done/stopped/error 帧或 10 分钟超时解除。
    // 误置（进程其实已退）的代价只是发送被禁用一段时间，优于消息被吞。
    if (history.length > 0) {
      const last = history[history.length - 1];
      if ((last.kind === 'tool_calls' || last.kind === 'tool_result') && !runningRef.current) {
        runningRef.current = true;
        setRunning(true);
        if (timeoutRef.current) clearTimeout(timeoutRef.current);
        timeoutRef.current = globalThis.setTimeout(() => {
          setItems((prev) => [...prev, { kind: 'system', systemTone: 'warning', content: tRef.current('agent.responseTimeout') }]);
          runningRef.current = false;
          setRunning(false);
          timeoutRef.current = null;
        }, RUNNING_TIMEOUT_MS);
      }
    }
    const loaded: ChatItem[] = [];
    // 新格式：kind='tool_calls' 行的原始调用记录，按 tool_call_id 关联 args；
    // 同时保留 ACP 新格式的 tool_kind/diffs/locations 供 tool_result 行合并
    const callArgs = new Map<string, {
      name: string;
      args: string;
      toolKind?: ToolKind;
      toolDiffs?: ToolDiff[];
      toolLocations?: ToolLocation[];
    }>();
    for (const m of history) {
      if (m.kind === 'tool_calls' && m.tool_calls) {
        try {
          const parsed = JSON.parse(m.tool_calls) as {
            id: string;
            function?: { name?: string; arguments?: string };
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
            });
          }
        } catch {
          /* ignore malformed tool_calls */
        }
      }
    }
    // 有配对 tool_result 的 tool_call_id 集合：tool_calls 行只为「无配对」的
    // 孤儿行渲染兜底卡片（正常完成的工具由 tool_result 卡片展示，不重复）。
    const pairedResultIds = new Set<string>();
    for (const m of history) {
      if (m.kind === 'tool_result' && m.tool_call_id) pairedResultIds.add(m.tool_call_id);
    }
    // 压缩重插去重：kept 段在 summary 前保留原始行（801c9a6），DB 物理顺序为
    // [..., 原kept, summary, 重插kept, 压缩后新消息...]，前端全量渲染会重复。
    // 不能用「summary 之后的行数」当作重插行数（压缩后新消息也排在 summary 后，
    // 会把行数放大、多跳掉没有重复副本的合法旧行）。改为内容匹配：对每个
    // summary，以「summary 后紧跟的重插段」为模板，从 summary 前紧邻行向前找
    // 等长且逐行全等（kind/role/content/tool_calls/tool_call_id/name）的连续
    // 段——重插段是 kept 段原样复制，故 summary 前必存在这样一段原件。
    // 重插段长度未知：先取「summary 后到下一个 summary/末尾」的行数作为上界，
    // 逐步缩短直到匹配上（首个全等的段长即 kept_count，余下的是压缩后新消息）。
    // 对每个 summary（含多次压缩）都做，因为每个 summary 各对应一次重插。
    const normNull = (v: unknown) => (v === undefined ? null : v);
    const rowEquals = (a: (typeof history)[number], b: (typeof history)[number]) =>
      a.kind === b.kind &&
      a.role === b.role &&
      a.content === b.content &&
      normNull(a.tool_calls) === normNull(b.tool_calls) &&
      normNull(a.tool_call_id) === normNull(b.tool_call_id) &&
      normNull(a.name) === normNull(b.name);
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
    // 历史中多条 plan 行只保留最后一条（ACP plan 全量替换语义）：先记录索引
    let lastPlanIdx = -1;
    for (let i = 0; i < history.length; i++) {
      const m = history[i];
      if (skipBeforeSummary.has(i)) continue;
      if (m.kind === 'tool_result') {
        const call: { name: string; args: string; toolKind?: ToolKind; toolDiffs?: ToolDiff[]; toolLocations?: ToolLocation[] } =
          (m.tool_call_id && callArgs.get(m.tool_call_id)) || { name: m.name ?? '', args: '' };
        loaded.push({
          kind: 'tool',
          content: '',
          toolName: call.name,
          toolArgs: call.args,
          toolResult: m.content,
          toolStatus: 'completed',
          toolKind: call.toolKind,
          toolDiffs: call.toolDiffs,
          toolLocations: call.toolLocations,
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
        // kind='tool_calls' 的孤儿行（回合中断在工具执行中：ToolCall 已落库，
        // 对应 ToolCallUpdate/tool_result 永不到达）。history effect 的注释说
        // 「kind='tool_calls' 行本身不渲染」——但那只在有配对 tool_result 时成立：
        // 正常路径下 tool_result 卡片已携带 args，无需重复渲染。孤儿行若没有
        // 任何卡片兜底，重载后该工具就从聊天区彻底消失（现象：卡片无标题无内容、
        // 或凭空少一段）。渲染为 failed 占位卡片，让用户看到中断痕迹。
        if (m.tool_call_id && !m.content && !pairedResultIds.has(m.tool_call_id)) {
          const call = callArgs.get(m.tool_call_id);
          if (call) {
            loaded.push({
              kind: 'tool',
              content: '',
              toolName: call.name,
              toolArgs: call.args,
              toolResult: undefined,
              toolStatus: 'failed',
              toolKind: call.toolKind,
              toolDiffs: call.toolDiffs,
              toolLocations: call.toolLocations,
            });
          }
        }
      } else if (m.kind === 'message' && m.name === 'thought' && m.content) {
        loaded.push({ kind: 'thought', content: m.content });
      } else if (m.kind === 'message' && m.name === 'plan') {
        // 只保留最后一条 plan（ACP plan 全量替换语义）：先记录索引，循环后处理
        lastPlanIdx = loaded.length;
        loaded.push({ kind: 'plan', content: '', planEntries: parsePlanEntries(m.content) });
      } else if (m.kind === 'message' && m.content) {
        loaded.push({ kind: m.role === 'user' ? 'user' : 'assistant', content: m.content });
      } else if (m.kind === 'summary' && m.content) {
        // summary 渲染为 assistant 气泡（muted 样式），避免与普通用户消息混淆
        loaded.push({ kind: 'assistant', content: m.content });
      }
      // kind='tool_calls' 行本身不渲染（args 已合并进 tool_result 卡片）
    }
    // 历史中多条 plan 行只渲染最后一条
    const finalLoaded = lastPlanIdx >= 0
      ? loaded.filter((it, i) => it.kind !== 'plan' || i === lastPlanIdx)
      : loaded;
    setItems(finalLoaded);
    // t 用于 running 超时提示文案；语言切换后重跑 effect 只影响尚未触发的
    // 超时回调文案，代价可忽略。
  }, [history, t]);

  const clearRunningTimeout = useCallback(() => {
    if (timeoutRef.current) {
      clearTimeout(timeoutRef.current);
      timeoutRef.current = null;
    }
  }, []);

  // running 的 ref 镜像供 onclose 闭包使用（onclose 里读不到最新 state）。
  // armRunning/stopRunning 是 useCallback，同步维护。
  // 把攒批的 chunk 缓冲一次性合并进流式气泡（新建或追加）。同步置 ref 保证
  // 与后续 setItems 更新的顺序一致（WS 回调在 React 外，flush 时机不依赖渲染）。
  const flushChunks = useCallback(() => {
    if (chunkFlushTimerRef.current) {
      clearTimeout(chunkFlushTimerRef.current);
      chunkFlushTimerRef.current = null;
    }
    const pending = chunkBufRef.current;
    if (!pending) return;
    chunkBufRef.current = '';
    const bubbleKind = streamingKindRef.current ?? 'assistant';
    setItems((prev) => {
      const idx = streamingIdxRef.current;
      if (idx !== null && prev[idx]?.kind === bubbleKind) {
        const next = [...prev];
        next[idx] = { ...next[idx], content: next[idx].content + pending };
        return next;
      }
      streamingIdxRef.current = prev.length;
      return [...prev, { kind: bubbleKind, content: pending }];
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

  const stopRunning = useCallback(() => {
    runningRef.current = false;
    setRunning(false);
    clearRunningTimeout();
    pendingToolsRef.current.clear();
  }, [clearRunningTimeout]);

  // 回合终态处理：done/stopped/error/本地停止/10 分钟超时都把仍在 pending 的审批
  // 卡片置为 expired。否则卡片永久 pending → hasPendingApproval 恒 true → 发送按钮
  // 被锁死（服务端 5 分钟审批超时实际按 deny 继续回合，UI 必须与服务端结果对齐）。
  // expired 与用户主动 denied 区分：被动过期（超时/终态）vs 主动拒绝。
  const expirePendingApprovals = useCallback(() => {
    setItems((prev) => prev.map((it) =>
      it.kind === 'approval' && it.approvalStatus === 'pending'
        ? { ...it, approvalStatus: 'expired' }
        : it
    ));
  }, []);

  const armRunning = useCallback(() => {
    if (runningRef.current) return;
    runningRef.current = true;
    setRunning(true);
    clearRunningTimeout();
    // 10 分钟超时兜底：到点未终态则强制解除（同时把 pending 审批置过期）
    timeoutRef.current = globalThis.setTimeout(() => {
      setItems((prev) => [...prev, { kind: 'system', systemTone: 'warning', content: tRef.current('agent.responseTimeout') }]);
      expirePendingApprovals();
      stopRunning();
    }, RUNNING_TIMEOUT_MS);
  }, [clearRunningTimeout, stopRunning, expirePendingApprovals]);

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
        if (needHistoryReload) {
          needHistoryReload = false;
          // 允许历史 effect 重新装载（与断线期间服务端已落库的内容对齐）
          loadedRef.current = false;
          void queryClient.invalidateQueries({ queryKey: ['agent-messages', sessionId] });
        }
      };

      ws.onmessage = (ev) => {
      let msg: AgentWsEvent;
      try {
        msg = JSON.parse(ev.data) as AgentWsEvent;
      } catch {
        return;
      }
      if (msg.type === 'assistant_chunk') {
        if (msg.content) {
          // thought 与正文分气泡：kind 切换先 flush 当前缓冲，但不断流——下方
          // streamingKindRef 更新为新 kind，下个 flush 的 `prev[idx].kind !==
          // bubbleKind` 检查天然新建气泡；此处若 breakStream 排队置 null 反而会
          // 在 updater 执行时把刚设的 streamingKindRef 清掉（M1）。
          const nextKind = msg.thought ? 'thought' : 'assistant';
          if (streamingKindRef.current !== null && streamingKindRef.current !== nextKind) {
            flushChunks();
          }
          streamingKindRef.current = nextKind;
          chunkBufRef.current += msg.content;
          scheduleChunkFlush();
        }
        if (msg.final) {
          // 收尾：先冲掉缓冲里的增量（同帧 content+final 的非 SSE 回退也在此落齐），
          // 再关闭流式气泡（ref 置 null 走更新队列，与 flush 的 ref 写入保持顺序）。
          flushChunks();
          breakStream();
        }
      } else if (msg.type === 'stream_reset') {
        // 上游流传输失败重试：丢弃已缓冲的半截增量，并真正移除已 flush 实体化
        // 的半截气泡，让重试的完整文本从新气泡开始（后续 status 帧会提示重试次数）。
        const idx = streamingIdxRef.current;
        chunkBufRef.current = '';
        flushChunks(); // 清缓冲后为 no-op，仅取消 pending flush 定时器
        breakStream();
        setItems((prev) => {
          if (idx !== null) {
            const k = prev[idx]?.kind;
            if (k === 'assistant' || k === 'thought') {
              return prev.filter((_, i) => i !== idx); // 真正移除半截气泡
            }
          }
          return prev;
        });
      } else if (msg.type === 'tool_call') {
        if (msg.id) {
          pendingTools.add(msg.id);
          toolIdsRef.current.push(msg.id);
        }
        // 服务端进入工具执行 → 显示 Running（对无前置 send 的乱序帧同样成立）
        armRunning();
        // 工具回合与文本回合交替：先冲掉缓冲里的文本增量（保证气泡顺序），
        // 再断开流式气泡追加工具卡片
        flushChunks();
        breakStream();
        setItems((prev) => [
          ...prev,
          {
            kind: 'tool',
            content: '',
            toolName: msg.name,
            toolArgs: msg.args,
            toolKind: msg.tool_kind,
            toolStatus: msg.status ?? 'in_progress',
            toolDiffs: msg.diffs,
            toolLocations: msg.locations,
          },
        ]);
      } else if (msg.type === 'tool_result') {
        if (msg.id) {
          pendingTools.delete(msg.id);
          toolIdsRef.current = toolIdsRef.current.filter((x) => x !== msg.id);
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
          // 优先按 name 匹配（runner/旧帧语义）
          if (msg.name) {
            for (let i = next.length - 1; i >= 0; i--) {
              if (next[i].kind === 'tool' && next[i].toolName === msg.name && next[i].toolResult == null) {
                patch(i);
                return next;
              }
            }
          }
          // name 缺失/未命中：按 id 回退——id 在 toolIdsRef 的序号对应倒序未完成卡片
          if (msg.id) {
            const pendingIdx: number[] = [];
            for (let i = 0; i < next.length; i++) {
              if (next[i].kind === 'tool' && next[i].toolResult == null) pendingIdx.push(i);
            }
            // toolIdsRef 已移除本 id；用 pendingTools 快照不可靠，直接按到达顺序：
            // 同名匹配失败后取最早未完成卡片（ACP 工具按序完成，最早未完成即当前）
            if (pendingIdx.length > 0) {
              patch(pendingIdx[0]);
            }
          }
          return next;
        });
      } else if (msg.type === 'plan') {
        flushChunks();
        breakStream();
        const entries = msg.entries ?? [];
        setItems((prev) => {
          // 就地更新最后一条 plan 气泡（ACP plan 是全量替换语义）；无则追加
          for (let i = prev.length - 1; i >= 0; i--) {
            if (prev[i].kind === 'plan') {
              const next = [...prev];
              next[i] = { ...next[i], planEntries: entries };
              return next;
            }
          }
          return [...prev, { kind: 'plan', content: '', planEntries: entries }];
        });
      } else if (msg.type === 'usage') {
        // MVP：仅实时推送不落库不渲染（保留帧类型兼容，静默忽略）
      } else if (msg.type === 'status') {
        // 轻量提示行（压缩等中间状态）：复用 assistant 气泡样式但标记 status；
        // 不进气泡流 → 冲掉缓冲后断开流式气泡再追加独立行
        flushChunks();
        breakStream();
        setItems((prev) => [...prev, { kind: 'system', systemTone: 'info', content: msg.message ?? '' }]);
      } else if (msg.type === 'stopped') {
        // 服务端确认取消（本连接或另一标签页发起的 cancel 都会广播到本连接的处理逻辑）
        flushChunks();
        breakStream();
        stopRunning();
        // 回合已终态，未响应的审批请求随回合作废 → 卡片过期
        expirePendingApprovals();
      } else if (msg.type === 'done') {
        // 终态：解除 Running。若在飞的工具帧随断线丢失，等回齐会把 UI 锁死
        // 10 分钟——done 到达即无条件解除（工具卡片增量渲染，无需等回齐）。
        flushChunks();
        breakStream();
        stopRunning();
        // 回合成功结束：服务端 5 分钟审批超时按 deny 继续回合，仍 pending 的
        // 卡片必须过期，否则 hasPendingApproval 恒 true 锁死发送按钮
        expirePendingApprovals();
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
        setConfigOptions(normalizeConfigOptions(msg.options));
        // 服务端权威状态到达 = 乐观更新确认生效：放弃回滚快照
        configRollbackRef.current = null;
      } else if (msg.type === 'current_mode_update') {
        // agent 侧自行切 mode（如 shift+tab）：同步 mode 项当前值
        setConfigOptions((prev) =>
          prev.map((o) =>
            o.category === 'mode' && msg.mode_id
              ? { ...o, currentValue: msg.mode_id }
              : o,
          ),
        );
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
        }]);
      } else if (msg.type === 'error') {
        // 「设置失败」error 帧（服务端 set_config_option 失败，格式 `设置失败: {e}`）：
        // 乐观更新从未生效，回滚到发送前快照，按钮不再显示假性值。
        if (configRollbackRef.current && msg.message?.startsWith('设置失败')) {
          setConfigOptions(configRollbackRef.current);
          configRollbackRef.current = null;
        }
        flushChunks();
        breakStream();
        setItems((prev) => [...prev, { kind: 'system', systemTone: 'error', content: msg.message ?? '' }]);
        stopRunning();
        // 回合以错误终态结束，未响应的审批卡片一并过期
        expirePendingApprovals();
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
            { kind: 'system', systemTone: 'warning', content: tRef.current('agent.connectionInterrupted') },
          ]);
        }
        stopRunning();
        // 断线时服务端 turn 被 drop、未响应审批按 deny 落定；本地卡片同样置
        // expired，否则重连后历史 refetch 失败会永久锁死发送按钮
        expirePendingApprovals();
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
    return () => {
      closedByCleanup = true;
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
      chunkBufRef.current = '';
      pendingTools.clear();
    };
  }, [sessionId, queryClient, armRunning, stopRunning, clearRunningTimeout, flushChunks, scheduleChunkFlush, expirePendingApprovals, breakStream]);

  useEffect(() => {
    // 仅当用户接近底部时才自动滚动（上翻读历史不被拽回）；直接滚动到底，
    // 避免逐 token smooth 动画互相堆积。jsdom 未实现 scrollIntoView，?.() 保底。
    if (stickToBottomRef.current) {
      bottomRef.current?.scrollIntoView?.({ behavior: 'auto' });
    }
  }, [items]);

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

  // 悬浮输入框高度测量（含长高/重连提示条出现）：消息区底部占位与渐隐随之伸缩
  useEffect(() => {
    const el = inputCardRef.current;
    if (!el || typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(() => setInputFloatH(el.offsetHeight));
    ro.observe(el);
    setInputFloatH(el.offsetHeight);
    return () => ro.disconnect();
  }, []);

  // 存在未响应的审批卡片时禁止继续发送（服务端在该审批响应前挂起回合）
  const hasPendingApproval = items.some((it) => it.kind === 'approval' && it.approvalStatus === 'pending');

  // 审批响应：approved=true 时 remember 决定「仅本次」还是「本会话记住」；
  // ACP options 路径由 ApprovalCard 传入 optionId（原样回传 option_id，后端优先
  // 解析），remember 对 allow_always 选项置 true。无论 WS 是否可用都先落本地
  // 状态（卡片从 pending 变 approved/denied）
  const respondApproval = (id: string, approved: boolean, remember: boolean, optionId?: string) => {
    const ws = wsRef.current;
    if (ws && ws.readyState === WebSocket.OPEN) {
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
    }
    setItems((prev) => prev.map((it) =>
      it.kind === 'approval' && it.approvalId === id
        ? { ...it, approvalStatus: approved ? 'approved' : 'denied' }
        : it
    ));
  };

  // @ 弹层触发检测：光标前找最近的 @（前面是空格/行首），其后到光标为 query。
  // 命中则打开弹层；query 含空白（@ 后直接空格）或光标前无 @ 则关闭。
  const handleInputChange = (e: React.ChangeEvent<HTMLTextAreaElement>) => {
    const v = e.target.value;
    setInput(v);
    const pos = e.target.selectionStart ?? v.length;
    const before = v.slice(0, pos);
    const at = before.lastIndexOf('@');
    if (at >= 0 && (at === 0 || /\s/.test(before[at - 1]))) {
      const q = before.slice(at + 1);
      if (!/\s/.test(q)) {
        setMention({ start: at, query: q });
        return;
      }
    }
    closeMention();
  };

  // 关闭 @ 弹层并清空受控高亮/列表状态：避免重开弹层时选中上一次的陈旧结果
  const closeMention = useCallback(() => {
    setMention(null);
    setMentionFiles([]);
    setMentionActiveIdx(0);
  }, []);

  // 选中文件：把 @query 段从文本移除，路径进 refs chip（chip 独立展示，不占 textarea）
  const selectMention = (path: string) => {
    if (!mention) return;
    const before = input.slice(0, mention.start);
    const after = input.slice(mention.start + 1 + mention.query.length);
    setInput(before + after);
    setRefs((prev) => (prev.includes(path) ? prev : [...prev, path]));
    closeMention();
    if (blurTimerRef.current) {
      clearTimeout(blurTimerRef.current);
      blurTimerRef.current = null;
    }
    textareaRef.current?.focus();
  };

  // 稳定回调（供 MentionPopup 的 effect 依赖）：setState 函数恒等，避免触发渲染循环
  const handleMentionFilesChange = useCallback((files: string[]) => {
    setMentionFiles(files);
  }, []);
  const handleMentionActiveIdxChange = useCallback((idx: number) => {
    setMentionActiveIdx(idx);
  }, []);

  const send = () => {
    const text = input.trim();
    if (!text || running || hasPendingApproval) return;
    const ws = wsRef.current;
    // WebSocket may be CONNECTING/CLOSED/CLOSING: sending throws InvalidStateError and
    // the message is silently lost, leaving running stuck true. Gate on OPEN instead.
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      setItems((prev) => [...prev, { kind: 'system', systemTone: 'error', content: t('agent.connectionLost') }]);
      return;
    }
    try {
      ws.send(JSON.stringify({ type: 'user_message', content: text, refs }));
    } catch {
      setItems((prev) => [...prev, { kind: 'system', systemTone: 'error', content: t('agent.connectionLost') }]);
      return;
    }
    setItems((prev) => [...prev, { kind: 'user', content: text }]);
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
    // 本地停止路径同样作废未响应的审批卡片（cancel 帧可能因断线永远不回来）
    expirePendingApprovals();
    setItems((prev) => [...prev, { kind: 'system', systemTone: 'stopped', content: t('agent.stopped') }]);
  };

  const handleModelChange = (id: string) => {
    const prev = model;
    onModelChange(id);
    void updateAgentSessionModel(sessionId, id)
      .then(() => {
        // 成功后 invalidate 会话列表缓存，让顶栏/会话列表的模型回显自愈
        void queryClient.invalidateQueries({ queryKey: ['agent-sessions'] });
      })
      .catch((err: unknown) => {
        // 失败：本地 state 回滚到旧值 + 用户可见错误提示
        onModelChange(prev);
        setItems((prevItems) => [
          ...prevItems,
          { kind: 'system', systemTone: 'error', content: `${t('agent.modelUpdateFailed')}: ${getApiErrorMessage(err)}` },
        ]);
      });
  };

  // ACP config option 切换：乐观更新 + WS 发送；发送失败或服务端「设置失败」
  // error 帧回滚（configRollbackRef 快照），生效确认以服务端回推的
  // config_option_update / session_state 全量帧为准。
  const sendConfigOption = (configId: string, value: string) => {
    const prev = configOptions;
    // 保留回滚快照；不在此清空——发送成功与否要等服务端权威确认帧
    configRollbackRef.current = prev;
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
      // 帧未发出：本地回滚即操作终结，快照一并作废
      configRollbackRef.current = null;
      setConfigOptions(prev);
      return;
    }
    try {
      ws.send(JSON.stringify({ type: 'set_config_option', config_id: configId, value }));
    } catch {
      // send 同步抛错：帧未到达服务端，回滚并作废快照
      configRollbackRef.current = null;
      setConfigOptions(prev);
      setItems((prevItems) => [
        ...prevItems,
        { kind: 'system', systemTone: 'error', content: t('agent.connectionLost') },
      ]);
    }
  };

  // mode/effort 走右侧快捷按钮（发送按钮左边）；其余 options 进左侧统一菜单
  const modeOption = configOptions.find((o) => o.category === 'mode');
  const effortOption = configOptions.find((o) => o.category === 'thought_level');
  const menuOptions = configOptions.filter(
    (o) => o.category !== 'mode' && o.category !== 'thought_level',
  );

  return (
    <div className="relative flex h-full flex-col">
      <div
        className="flex-1 space-y-3 overflow-y-auto px-3 py-3 md:space-y-4 md:px-5 md:py-4"
        onScroll={(e) => {
          const el = e.currentTarget;
          // 距底 < 80px 视为「跟随流式输出」；上翻超过阈值即停止自动滚动
          stickToBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
        }}
      >
        {items.length === 0 && !running && (
          <p className="text-center text-sm text-muted-foreground">{t('agent.chatEmptyHint')}</p>
        )}
        {items.map((it, i) => (
          it.kind === 'system'
            ? <SystemMessage key={i} tone={it.systemTone} content={it.content} />
            : it.kind === 'approval'
              ? <ApprovalCard key={i} item={it} onRespond={respondApproval} />
              : <MessageBubble key={i} item={it} />
        ))}
        {running && (
          <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            {t('agent.running')}
          </div>
        )}
        {/* 悬浮输入框占位：保证最后一条消息能滚动到输入框之上 */}
        <div aria-hidden style={{ height: inputFloatH + 8 }} />
        <div ref={bottomRef} />
      </div>

      {/* 底部渐隐：消息滑入悬浮输入框下方时柔和淡出 */}
      <div
        aria-hidden
        className="pointer-events-none absolute inset-x-0 bottom-0 bg-gradient-to-t from-card via-card/85 to-transparent"
        style={{ height: inputFloatH + 28 }}
      />

      {/* 悬浮输入框（VS Code Claude Code 风格）：模型选择(左下) + 发送图标(右下) 内嵌 */}
      <div className="absolute inset-x-0 bottom-0 px-3 pb-[max(env(safe-area-inset-bottom),0.75rem)] md:px-6 md:pb-5">
        <div ref={inputCardRef} className="mx-auto w-full max-w-3xl">
        {disconnected && (
          <div className="mb-1 flex items-center gap-1.5 rounded-md bg-destructive/10 px-2.5 py-1.5 text-xs text-destructive md:mb-1.5">
            <Loader2 className="h-3 w-3 animate-spin" />
            {t('agent.reconnecting')}
          </div>
        )}
        <div className="relative rounded-2xl border border-input bg-background shadow-2xl focus-within:ring-1 focus-within:ring-ring">
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
            />
          )}
          <textarea
            ref={textareaRef}
            value={input}
            onChange={handleInputChange}
            onKeyDown={(e) => {
              // IME 组词中（拼音候选窗）的按键不触发任何快捷键：回车是确认候选而非发送
              if (e.nativeEvent.isComposing) return;
              if (e.key === 'Escape') {
                closeMention();
                return;
              }
              if (mention) {
                // 弹层打开时键盘操作：↑↓ 循环移动高亮、Enter/Tab 选中、Shift+Enter 放行换行
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
            }}
            onFocus={() => {
              // 用户回到输入框（或弹层项选中后主动 focus）→ 取消待执行的关闭
              if (blurTimerRef.current) {
                clearTimeout(blurTimerRef.current);
                blurTimerRef.current = null;
              }
            }}
            placeholder={t('agent.inputPlaceholder')}
            className="w-full min-h-[3.5rem] resize-none rounded-t-2xl border-0 bg-transparent px-3 pb-1 pt-2 text-sm leading-5 focus:outline-none"
            rows={1}
          />
          <div className="flex flex-wrap items-center justify-between gap-1 px-1.5 pb-1.5 md:px-2">
            <SessionSettingsMenu
              model={model}
              onModelChange={handleModelChange}
              configOptions={menuOptions}
              onConfigChange={sendConfigOption}
              disabled={running}
            />
            <div className="flex items-center gap-0.5">
              <ConfigOptionButton
                option={modeOption}
                label="agent.configMode"
                onChange={sendConfigOption}
                disabled={running}
                placeholder={configOptions.length > 0 && !modeOption}
              />
              <ConfigOptionButton
                option={effortOption}
                label="agent.configEffort"
                onChange={sendConfigOption}
                disabled={running}
                placeholder={configOptions.length > 0 && !effortOption}
              />
              {running ? (
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
                  disabled={!input.trim() || hasPendingApproval}
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
  );
}
