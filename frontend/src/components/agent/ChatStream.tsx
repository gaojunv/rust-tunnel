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
import type { ChatItem } from './types';
import ApprovalCard from './ApprovalCard';
import MessageBubble from './MessageBubble';
import ModelSelect from './ModelSelect';

const RUNNING_TIMEOUT_MS = 10 * 60 * 1000; // 10 分钟兜底
/** 流式 chunk 合并 flush 间隔：token 级 WS 帧攒批后一次性写 state，避免每 token 全列表重渲染。 */
const STREAM_FLUSH_MS = 50;

interface Props {
  sessionId: string;
  model: string;
  onModelChange: (id: string) => void;
}

export default function ChatStream({ sessionId, model, onModelChange }: Props) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [items, setItems] = useState<ChatItem[]>([]);
  const [input, setInput] = useState('');
  const [running, setRunning] = useState(false);
  const [disconnected, setDisconnected] = useState(false);
  const wsRef = useRef<WebSocket | null>(null);
  const bottomRef = useRef<HTMLDivElement>(null);
  // 历史只在挂载时装载一次：refetch（done 后 invalidate）会改写聊天区，
  // 而对话中新增的 item 是会话内的实时增量，不能用服务器历史整体覆盖。
  const loadedRef = useRef(false);
  // 在飞工具调用（按 id 追踪），running 解除需其清空
  const pendingToolsRef = useRef<Set<string>>(new Set());
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // running 的 ref 镜像：WS onmessage 闭包内避免读旧 state
  const runningRef = useRef(false);
  // 当前正在流式写入的气泡 index（assistant_chunk 增量合并用；final/新事件到达时置 null）
  const streamingIdxRef = useRef<number | null>(null);
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
    if (!history || loadedRef.current) return;
    loadedRef.current = true;
    const loaded: ChatItem[] = [];
    // 新格式：kind='tool_calls' 行的原始调用记录，按 tool_call_id 关联 args
    const callArgs = new Map<string, { name: string; args: string }>();
    for (const m of history) {
      if (m.kind === 'tool_calls' && m.tool_calls) {
        try {
          for (const c of JSON.parse(m.tool_calls) as { id: string; function?: { name?: string; arguments?: string } }[]) {
            callArgs.set(c.id, { name: c.function?.name ?? '', args: c.function?.arguments ?? '' });
          }
        } catch {
          /* ignore malformed tool_calls */
        }
      }
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
    for (let i = 0; i < history.length; i++) {
      const m = history[i];
      if (skipBeforeSummary.has(i)) continue;
      if (m.kind === 'tool_result') {
        const call = (m.tool_call_id && callArgs.get(m.tool_call_id)) || { name: m.name ?? '', args: '' };
        loaded.push({ kind: 'tool', content: '', toolName: call.name, toolArgs: call.args, toolResult: m.content });
      } else if ((m.kind === 'tool' || m.role === 'tool') && m.tool_calls) {
        // 旧格式：合并 tool_log JSON 行
        try {
          for (const t of JSON.parse(m.tool_calls)) {
            loaded.push({ kind: 'tool', content: '', toolName: t.name, toolArgs: t.args, toolResult: t.result });
          }
        } catch {
          /* ignore malformed tool_calls */
        }
      } else if (m.kind === 'message' && m.content) {
        loaded.push({ kind: m.role === 'user' ? 'user' : 'assistant', content: m.content });
      } else if (m.kind === 'summary' && m.content) {
        // summary 渲染为 assistant 气泡（muted 样式），避免与普通用户消息混淆
        loaded.push({ kind: 'assistant', content: m.content });
      }
      // kind='tool_calls' 行本身不渲染（args 已合并进 tool_result 卡片）
    }
    setItems(loaded);
  }, [history]);

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
    setItems((prev) => {
      const idx = streamingIdxRef.current;
      if (idx !== null && prev[idx]?.kind === 'assistant') {
        const next = [...prev];
        next[idx] = { ...next[idx], content: next[idx].content + pending };
        return next;
      }
      streamingIdxRef.current = prev.length;
      return [...prev, { kind: 'assistant', content: pending }];
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
      setItems((prev) => [...prev, { kind: 'assistant', content: `⚠️ ${t('agent.responseTimeout')}` }]);
      expirePendingApprovals();
      stopRunning();
    }, RUNNING_TIMEOUT_MS);
  }, [clearRunningTimeout, stopRunning, expirePendingApprovals, t]);

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
          // 攒批：先入缓冲，节流 flush（避免每 token 全列表重渲染）
          chunkBufRef.current += msg.content;
          scheduleChunkFlush();
        }
        if (msg.final) {
          // 收尾：先冲掉缓冲里的增量（同帧 content+final 的非 SSE 回退也在此落齐），
          // 再关闭流式气泡（ref 置 null 走更新队列，与 flush 的 ref 写入保持顺序）。
          flushChunks();
          setItems((prev) => {
            streamingIdxRef.current = null;
            return prev;
          });
        }
      } else if (msg.type === 'tool_call') {
        if (msg.id) pendingTools.add(msg.id);
        // 服务端进入工具执行 → 显示 Running（对无前置 send 的乱序帧同样成立）
        armRunning();
        // 工具回合与文本回合交替：先冲掉缓冲里的文本增量（保证气泡顺序），
        // 再断开流式气泡追加工具卡片
        flushChunks();
        setItems((prev) => {
          streamingIdxRef.current = null;
          return [...prev, { kind: 'tool', content: '', toolName: msg.name, toolArgs: msg.args }];
        });
      } else if (msg.type === 'tool_result') {
        if (msg.id) pendingTools.delete(msg.id);
        setItems((prev) => {
          const next = [...prev];
          for (let i = next.length - 1; i >= 0; i--) {
            if (next[i].kind === 'tool' && next[i].toolName === msg.name && !next[i].toolResult) {
              next[i] = { ...next[i], toolResult: msg.result };
              break;
            }
          }
          return next;
        });
      } else if (msg.type === 'status') {
        // 轻量提示行（压缩等中间状态）：复用 assistant 气泡样式但标记 status；
        // 不进气泡流 → 冲掉缓冲后断开流式气泡再追加独立行
        flushChunks();
        setItems((prev) => {
          streamingIdxRef.current = null;
          return [...prev, { kind: 'assistant', content: `ℹ️ ${msg.message ?? ''}` }];
        });
      } else if (msg.type === 'stopped') {
        // 服务端确认取消（本连接或另一标签页发起的 cancel 都会广播到本连接的处理逻辑）
        flushChunks();
        setItems((prev) => {
          streamingIdxRef.current = null;
          return prev;
        });
        stopRunning();
        // 回合已终态，未响应的审批请求随回合作废 → 卡片过期
        expirePendingApprovals();
      } else if (msg.type === 'done') {
        // 终态：解除 Running。若在飞的工具帧随断线丢失，等回齐会把 UI 锁死
        // 10 分钟——done 到达即无条件解除（工具卡片增量渲染，无需等回齐）。
        flushChunks();
        setItems((prev) => {
          streamingIdxRef.current = null;
          return prev;
        });
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
      } else if (msg.type === 'approval_request') {
        // 危险操作审批：先冲掉缓冲里的文本增量，再追加审批卡片（等待用户响应）
        flushChunks();
        setItems((prev) => {
          streamingIdxRef.current = null;
          return [...prev, {
            kind: 'approval',
            content: '',
            approvalId: msg.request_id,
            approvalTool: msg.tool,
            approvalSummary: msg.summary,
            approvalStatus: 'pending',
          }];
        });
      } else if (msg.type === 'error') {
        flushChunks();
        setItems((prev) => {
          streamingIdxRef.current = null;
          return [...prev, { kind: 'assistant', content: `⚠️ ${msg.message}` }];
        });
        stopRunning();
        // 回合以错误终态结束，未响应的审批卡片一并过期
        expirePendingApprovals();
      }
    };

      ws.onclose = () => {
        wsRef.current = null;
        if (closedByCleanup) return;
        // 断线：本地回合状态作废（服务端可能还在跑，也可能已丢），重连后按
        // DB 历史对齐。用户消息已发出去但服务端未必收到——提示而非静默重发。
        if (runningRef.current) {
          setItems((prev) => [
            ...prev,
            { kind: 'assistant', content: `⚠️ ${t('agent.connectionInterrupted')}` },
          ]);
        }
        stopRunning();
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
  }, [sessionId, queryClient, armRunning, stopRunning, clearRunningTimeout, flushChunks, scheduleChunkFlush, expirePendingApprovals, t]);

  useEffect(() => {
    // 仅当用户接近底部时才自动滚动（上翻读历史不被拽回）；直接滚动到底，
    // 避免逐 token smooth 动画互相堆积。jsdom 未实现 scrollIntoView，?.() 保底。
    if (stickToBottomRef.current) {
      bottomRef.current?.scrollIntoView?.({ behavior: 'auto' });
    }
  }, [items]);

  // 存在未响应的审批卡片时禁止继续发送（服务端在该审批响应前挂起回合）
  const hasPendingApproval = items.some((it) => it.kind === 'approval' && it.approvalStatus === 'pending');

  // 审批响应：approved=true 时 remember 决定「仅本次」还是「本会话记住」；
  // 无论 WS 是否可用都先落本地状态（卡片从 pending 变 approved/denied）
  const respondApproval = (id: string, approved: boolean, remember: boolean) => {
    const ws = wsRef.current;
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({
        type: 'approval_response',
        request_id: id,
        approved,
        remember: remember ? 'session' : 'none',
      }));
    }
    setItems((prev) => prev.map((it) =>
      it.kind === 'approval' && it.approvalId === id
        ? { ...it, approvalStatus: approved ? 'approved' : 'denied' }
        : it
    ));
  };

  const send = () => {
    const text = input.trim();
    if (!text || running || hasPendingApproval) return;
    const ws = wsRef.current;
    // WebSocket may be CONNECTING/CLOSED/CLOSING: sending throws InvalidStateError and
    // the message is silently lost, leaving running stuck true. Gate on OPEN instead.
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      setItems((prev) => [...prev, { kind: 'assistant', content: `⚠️ ${t('agent.connectionLost')}` }]);
      return;
    }
    try {
      ws.send(JSON.stringify({ type: 'user_message', content: text }));
    } catch {
      setItems((prev) => [...prev, { kind: 'assistant', content: `⚠️ ${t('agent.connectionLost')}` }]);
      return;
    }
    setItems((prev) => [...prev, { kind: 'user', content: text }]);
    setInput('');
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
    stopRunning();
    // 本地停止路径同样作废未响应的审批卡片（cancel 帧可能因断线永远不回来）
    expirePendingApprovals();
    setItems((prev) => [...prev, { kind: 'assistant', content: `⏹️ ${t('agent.stopped')}` }]);
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
          { kind: 'assistant', content: `⚠️ ${t('agent.modelUpdateFailed')}: ${getApiErrorMessage(err)}` },
        ]);
      });
  };

  return (
    <div className="flex h-full flex-col">
      <div
        className="flex-1 space-y-3 overflow-y-auto p-4"
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
          it.kind === 'approval'
            ? <ApprovalCard key={i} item={it} onRespond={respondApproval} />
            : <MessageBubble key={i} item={it} />
        ))}
        {running && (
          <div className="flex items-center gap-1 text-sm text-muted-foreground">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            {t('agent.running')}
          </div>
        )}
        <div ref={bottomRef} />
      </div>

      {/* 一体化输入框：模型选择(左下) + 发送图标(右下) 内嵌 */}
      <div className="border-t p-2">
        {disconnected && (
          <div className="mb-1.5 flex items-center gap-1.5 rounded-md bg-destructive/10 px-2.5 py-1.5 text-xs text-destructive">
            <Loader2 className="h-3 w-3 animate-spin" />
            {t('agent.reconnecting')}
          </div>
        )}
        <div className="rounded-xl border border-input bg-background focus-within:ring-1 focus-within:ring-ring">
          <textarea
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey) {
                e.preventDefault();
                send();
              }
            }}
            placeholder={t('agent.inputPlaceholder')}
            className="w-full resize-none rounded-t-xl border-0 bg-transparent px-3 pt-2 text-sm focus:outline-none"
            rows={2}
          />
          <div className="flex items-center justify-between px-2 pb-1.5">
            <ModelSelect value={model} onChange={handleModelChange} disabled={running} />
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
  );
}
