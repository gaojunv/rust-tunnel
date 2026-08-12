import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useVirtualizer } from '@tanstack/react-virtual';
import { Button } from '@/components/ui/button';
import { Loader2, SendHorizontal, Square } from 'lucide-react';
import {
  agentWsUrl,
  getApiErrorMessage,
  listAgentMessages,
  updateAgentSessionModel,
} from '../../api/client';
import type { AgentWsEvent } from '../../types';
import type { ChatItem } from './types';
import ApprovalCard from './ApprovalCard';
import MentionPopup from './MentionPopup';
import MessageBubble from './MessageBubble';
import SessionSettingsMenu from './SessionSettingsMenu';
import SubagentTaskCard from './SubagentTaskCard';
import SystemMessage from './SystemMessage';
import ConfigOptionButton from './ConfigOptionButton';
import { normalizeConfigOptions } from './sessionConfig';
import { historyToChatItems } from './history';
import {
  appendChildStream,
  chunkKey,
  parseChunkKey,
  patchChildToolResult,
  upsertToolCard,
} from './subagent';
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
  // 消息区滚动容器 ref：虚拟化的 getScrollElement 目标。
  const scrollRef = useRef<HTMLDivElement>(null);
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
    // done 后的对账重载（见 done 处理器）：只重建 items、跳过 running 兜底——
    // 该重载的末行可能是 tool_result（回合在工具执行中结束），按现状会误置
    // running=true 并锁死发送按钮 10 分钟，而回合其实已终态。
    const isReconcileReload = reconcileRef.current;
    reconcileRef.current = false;
    if (!isReconcileReload && history.length > 0) {
      // 装载历史时若末尾是 tool_calls/tool_result 行，说明上次回合可能在工具执行中
      // 被打断（刷新/断线/服务端崩溃）。ACP 会话进程可能仍在跑（busy=true）。把
      // running 置 true 让用户看到「回合可能仍在执行」，直到 done/stopped/error 帧
      // 或 10 分钟超时解除。运行中发送已放开（服务端 busy 会排队），误置的代价只是
      // 指示器多亮一阵、消息走排队路径，优于用户以为回合已结束而重复发送。
      const last = history[history.length - 1];
      if ((last.kind === 'tool_calls' || last.kind === 'tool_result') && !runningRef.current) {
        // 半截装载标记：done 到达时允许 refetch 重渲染完整历史（对账）
        partialLoadRef.current = true;
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
    // 纯函数装载：历史行 → ChatItem（含同一 tool_call_id 多行去重、压缩重插
    // 去重、plan 只留最后一条），见 history.ts。
    setItems(historyToChatItems(history));
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
            next = [...next, { kind, content }];
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
          next = [...next, { kind, content, parentToolId: parent }];
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
          if (idx !== null) {
            const k = prev[idx]?.kind;
            if (k === 'assistant' || k === 'thought') {
              return prev.filter((_, i) => i !== idx); // 真正移除半截气泡
            }
          }
          return prev;
        });
      } else if (msg.type === 'tool_call') {
        const parentToolId = msg.parent_tool_call_id;
        const isSubagentCard = msg.is_subagent === true;
        // 子 agent 内部工具不 gate 主回合 running（父 Task 卡自身 tool_call 已
        // armRunning）；仅顶层工具（含父 Task 卡）追踪在飞 id
        if (!parentToolId) {
          if (msg.id) {
            pendingTools.add(msg.id);
            toolIdsRef.current.push(msg.id);
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
            next[parentIdx] = {
              ...next[parentIdx],
              children: upsertToolCard(next[parentIdx].children ?? [], toolItem),
            };
            return next;
          });
        } else {
          breakStream();
          setItems((prev) => {
            // 去重：刷新/重连时 live tool_call 可能与 history 已渲染的卡片是同一
            // 工具（tool_call 已落库、tool_result 未到）。按 toolId 就地升级
            // （upsertToolCard：覆盖历史误判的 failed 状态、保留已收纳 children），
            // 而不是再追加一张重复卡——否则 tool_result 只 patch 一张，另一张永远
            // running。
            // 子 agent 父卡到达：先收集此前平铺在顶层的孤儿子项（parentToolId 命中
            // 本卡 toolId），从主流移除并作为 children 挂载（「孤儿收纳」），保证
            // 先到子事件最终收纳进父卡、不重复、不丢失。
            const orphanKids = toolItem.toolId
              ? prev.filter((it) => it.parentToolId === toolItem.toolId)
              : [];
            const filtered =
              orphanKids.length > 0
                ? prev.filter((it) => it.parentToolId !== toolItem.toolId)
                : prev;
            return upsertToolCard(filtered, {
              ...toolItem,
              ...(orphanKids.length > 0 ? { children: orphanKids } : {}),
            });
          });
        }
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
        }
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
      } else if (msg.type === 'queued') {
        // 运行中提交消息 → 服务端 busy 入队确认：轻量提示（不打断当前流式气泡）。
        // 队列在服务端（前端不做本地排队），本连接后续会收到该消息对应的流式帧。
        setItems((prev) => [...prev, { kind: 'system', systemTone: 'info', content: tRef.current('agent.messageQueued') }]);
      } else if (msg.type === 'stopped') {
        // 服务端确认取消（本连接或另一标签页发起的 cancel 都会广播到本连接的处理逻辑）
        flushChunks();
        breakStream();
        stopRunning();
        // 回合已终态，未响应的审批请求随回合作废 → 卡片过期
        expirePendingApprovals();
      } else if (msg.type === 'cancel_fallback') {
        // 停止超时兜底：agent 进程未在时限内退出，服务端强制杀掉并重启。
        // 当前回合已死（上下文可能丢失）——按终态处理并提示用户。
        flushChunks();
        breakStream();
        stopRunning();
        expirePendingApprovals();
        setItems((prev) => [...prev, { kind: 'system', systemTone: 'warning', content: tRef.current('agent.cancelFallback') }]);
      } else if (msg.type === 'done') {
        // 终态：解除 Running。若在飞的工具帧随断线丢失，等回齐会把 UI 锁死
        // 10 分钟——done 到达即无条件解除（工具卡片增量渲染，无需等回齐）。
        flushChunks();
        breakStream();
        stopRunning();
        // 回合成功结束：服务端 5 分钟审批超时按 deny 继续回合，仍 pending 的
        // 卡片必须过期，否则 hasPendingApproval 恒 true 锁死发送按钮
        expirePendingApprovals();
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
      chunkBufRef.current = new Map();
      pendingTools.clear();
    };
  }, [sessionId, queryClient, armRunning, stopRunning, clearRunningTimeout, flushChunks, scheduleChunkFlush, expirePendingApprovals, breakStream, breakSubStream]);

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
    // 运行中不再短路：服务端 busy 时会排队（回 queued 帧），消息不会丢。
    // hasPendingApproval 仍需阻塞——服务端在该审批响应前挂起回合，发送必被吞。
    if (!text || hasPendingApproval) return;
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
      return <SystemMessage key={i} tone={it.systemTone} content={it.content} />;
    }
    if (it.kind === 'approval') {
      return <ApprovalCard key={it.approvalId ?? i} item={it} onRespond={respondApproval} />;
    }
    if (it.kind === 'tool' && (it.isSubagent || (it.children && it.children.length > 0))) {
      return (
        <SubagentTaskCard
          key={it.toolId ?? i}
          item={it}
          streamingChildIdx={it.toolId ? subStreamRef.current.get(it.toolId)?.idx : undefined}
        />
      );
    }
    return (
      <MessageBubble
        key={it.kind === 'tool' && it.toolId ? it.toolId : i}
        item={it}
        streaming={isStreaming}
      />
    );
  };

  return (
    <div className="relative flex h-full flex-col">
      <div
        ref={scrollRef}
        className="flex-1 overflow-y-auto px-3 py-3 md:px-5 md:py-4 dark:text-foreground/85"
        onScroll={(e) => {
          const el = e.currentTarget;
          // 距底 < 80px 视为「跟随流式输出」；上翻超过阈值即停止自动滚动
          stickToBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
        }}
      >
        {/* 限宽包裹层：与下方悬浮输入框的 max-w-3xl 对齐，控制长文阅读行长，
            避免消息流全宽铺开与居中输入框的视觉错位。渐隐占位留在层外保持全宽。 */}
        <div className="mx-auto w-full max-w-3xl">
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
        {running && (
          <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            {t('agent.running')}
          </div>
        )}
        <div ref={bottomRef} />
        </div>
        {/* 悬浮输入框占位 + 底部渐隐（合并为一个 sticky 元素）：sticky 固定在可视
            底部，高度同时充当占位，保证最后一条消息能滚动到输入框之上。作为滚动
            容器的子元素，浏览器把滚动条绘制在所有后代之上——渐隐不再遮挡滚动条、
            也不阻断其交互（此前 external absolute + inset-x-0 会盖住右侧细滚动条，
            导致 thumb 被渐变挡住、滚动条拖不动）；宽度自动等于内容宽度（不含
            滚动条），无需硬编码滚动条宽度。 */}
        <div
          aria-hidden
          className="pointer-events-none sticky bottom-0 bg-gradient-to-t from-card via-card/85 to-transparent"
          style={{ height: inputFloatH + 28 }}
        />
      </div>

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
            />
            <div className="flex items-center gap-0.5">
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
              {running && (
                <Button
                  onClick={stop}
                  size="sm"
                  variant="ghost"
                  aria-label={t('agent.stop')}
                  className="h-8 w-8 rounded-full p-0 text-destructive hover:text-destructive"
                >
                  <Square className="h-4 w-4 fill-current" />
                </Button>
              )}
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
            </div>
          </div>
        </div>
        </div>
      </div>
    </div>
  );
}
