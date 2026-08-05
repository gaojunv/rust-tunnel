import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Button } from '@/components/ui/button';
import { Loader2, SendHorizontal, Wrench } from 'lucide-react';
import {
  agentWsUrl,
  getApiErrorMessage,
  listAgentMessages,
  updateAgentSessionModel,
} from '../../api/client';
import type { AgentWsEvent } from '../../types';
import Markdown from './Markdown';
import ModelSelect from './ModelSelect';

interface ChatItem {
  kind: 'user' | 'assistant' | 'tool';
  content: string;
  toolName?: string;
  toolArgs?: string;
  toolResult?: string;
}

const RUNNING_TIMEOUT_MS = 10 * 60 * 1000; // 10 分钟兜底

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

  // 历史消息（与 ActivityBar 的 Git 面板共享 queryKey，invalidate 后自动刷新）
  const { data: history } = useQuery({
    queryKey: ['agent-messages', sessionId],
    queryFn: () => listAgentMessages(sessionId),
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  });
  useEffect(() => {
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
    // [..., 原kept, summary, 重插kept...]，前端全量渲染会重复。重插行数 =
    // summary 之后的行数 K（含 tool_calls/tool_result，与后端 kept_count 口径
    // 一致），故跳过 summary 前的最后 K 行即可去掉重复副本（多次压缩同样成立：
    // 每次压缩都恰把 summary 前最后 kept_count 行重插到 summary 后）。
    const skipBeforeLastSummary = new Set<number>();
    {
      let summaryIdx = -1;
      for (let i = history.length - 1; i >= 0; i--) {
        if (history[i].kind === 'summary') {
          summaryIdx = i;
          break;
        }
      }
      if (summaryIdx >= 0) {
        const k = history.length - summaryIdx - 1;
        for (let i = summaryIdx - 1; i >= Math.max(0, summaryIdx - k); i--) {
          skipBeforeLastSummary.add(i);
        }
      }
    }
    for (let i = 0; i < history.length; i++) {
      const m = history[i];
      if (skipBeforeLastSummary.has(i)) continue;
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

  const stopRunning = useCallback(() => {
    runningRef.current = false;
    setRunning(false);
    clearRunningTimeout();
    pendingToolsRef.current.clear();
  }, [clearRunningTimeout]);

  const armRunning = useCallback(() => {
    if (runningRef.current) return;
    runningRef.current = true;
    setRunning(true);
    clearRunningTimeout();
    // 10 分钟超时兜底：到点未终态则强制解除
    timeoutRef.current = globalThis.setTimeout(() => {
      setItems((prev) => [...prev, { kind: 'assistant', content: `⚠️ ${t('agent.responseTimeout')}` }]);
      stopRunning();
    }, RUNNING_TIMEOUT_MS);
  }, [clearRunningTimeout, stopRunning, t]);

  // WebSocket
  useEffect(() => {
    const ws = new WebSocket(agentWsUrl(sessionId));
    // ref 在组件生命周期内恒定，复制到局部变量供 handler/cleanup 使用（exhaustive-deps）
    const pendingTools = pendingToolsRef.current;
    wsRef.current = ws;
    ws.onmessage = (ev) => {
      let msg: AgentWsEvent;
      try {
        msg = JSON.parse(ev.data) as AgentWsEvent;
      } catch {
        return;
      }
      if (msg.type === 'assistant_chunk') {
        if (msg.content) {
          setItems((prev) => {
            const idx = streamingIdxRef.current;
            if (idx !== null && prev[idx]?.kind === 'assistant') {
              // 已有流式气泡 → 增量合并
              const next = [...prev];
              next[idx] = { ...next[idx], content: next[idx].content + msg.content! };
              return next;
            }
            streamingIdxRef.current = prev.length;
            return [...prev, { kind: 'assistant', content: msg.content! }];
          });
        }
        if (msg.final) {
          // 回合收尾：关闭也走更新队列，与增量合并的 ref 写入保持顺序。
          // （若在 emit 时同步置 null，React 批量 flush 时可能截断同批次尚未合并的增量。
          //   非 SSE 回退 content+final:true 同条 → 先追加再关闭，语义一致。）
          setItems((prev) => {
            streamingIdxRef.current = null;
            return prev;
          });
        }
      } else if (msg.type === 'tool_call') {
        if (msg.id) pendingTools.add(msg.id);
        // 服务端进入工具执行 → 显示 Running（对无前置 send 的乱序帧同样成立）
        armRunning();
        // 工具回合与文本回合交替：文本气泡必须断开（在更新队列内置 null，
        // 与增量合并的 ref 写入保持顺序，避免批量 flush 时截断合并）
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
        // 不进气泡流 → 在更新队列内先关闭当前流式气泡再追加独立行
        setItems((prev) => {
          streamingIdxRef.current = null;
          return [...prev, { kind: 'assistant', content: `ℹ️ ${msg.message ?? ''}` }];
        });
      } else if (msg.type === 'done') {
        // 严格终态：工具全部回齐才解除 Running（防御乱序帧）
        // 关闭流式气泡（返回原 prev → React 跳过重渲染，仅执行 ref 关闭）
        setItems((prev) => {
          streamingIdxRef.current = null;
          return prev;
        });
        if (pendingTools.size === 0) {
          stopRunning();
          // 刷新共享的历史缓存，让 ActivityBar 的 Git 面板拿到最新 tool 结果
          void queryClient.invalidateQueries({ queryKey: ['agent-messages', sessionId] });
        }
      } else if (msg.type === 'error') {
        setItems((prev) => {
          streamingIdxRef.current = null;
          return [...prev, { kind: 'assistant', content: `⚠️ ${msg.message}` }];
        });
        stopRunning();
      }
    };
    ws.onclose = () => stopRunning();
    ws.onerror = () => stopRunning();
    return () => {
      ws.onclose = null;
      ws.onerror = null;
      ws.close();
      clearRunningTimeout();
      pendingTools.clear();
    };
  }, [sessionId, queryClient, armRunning, stopRunning, clearRunningTimeout]);

  useEffect(() => {
    // jsdom 未实现 scrollIntoView，?.() 保证测试环境不抛错
    bottomRef.current?.scrollIntoView?.({ behavior: 'smooth' });
  }, [items]);

  const send = () => {
    const text = input.trim();
    if (!text || running) return;
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
      <div className="flex-1 space-y-3 overflow-y-auto p-4">
        {items.length === 0 && !running && (
          <p className="text-center text-sm text-muted-foreground">{t('agent.chatEmptyHint')}</p>
        )}
        {items.map((it, i) => (
          <div
            key={i}
            className={
              it.kind === 'user'
                ? 'ml-auto max-w-[80%] rounded-lg bg-primary/10 px-3 py-2'
                : it.kind === 'assistant'
                  ? 'mr-auto max-w-[80%] rounded-lg bg-muted px-3 py-2'
                  : 'mr-auto max-w-[90%] rounded-lg border bg-background px-3 py-2 text-sm font-mono'
            }
          >
            {it.kind === 'tool' ? (
              <div>
                <div className="mb-1 flex items-center gap-1 text-xs font-semibold">
                  <Wrench className="h-3.5 w-3.5 text-primary" />
                  {it.toolName}
                </div>
                {it.toolArgs && (
                  <pre className="whitespace-pre-wrap text-xs text-muted-foreground">{it.toolArgs}</pre>
                )}
                {it.toolResult ? (
                  <pre className="mt-2 whitespace-pre-wrap border-t pt-2 text-xs text-muted-foreground">
                    {it.toolResult}
                  </pre>
                ) : (
                  <div className="mt-1 flex items-center gap-1 text-xs text-muted-foreground">
                    <Loader2 className="h-3 w-3 animate-spin" />
                    {t('agent.toolRunning')}
                  </div>
                )}
              </div>
            ) : it.kind === 'assistant' ? (
              <Markdown content={it.content} />
            ) : (
              <div className="whitespace-pre-wrap">{it.content}</div>
            )}
          </div>
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
            <Button
              onClick={send}
              disabled={running || !input.trim()}
              size="sm"
              variant="ghost"
              aria-label={t('agent.send')}
              className="h-8 w-8 rounded-full p-0"
            >
              {running ? (
                <Loader2 className="h-4 w-4 animate-spin" />
              ) : (
                <SendHorizontal className="h-4 w-4" />
              )}
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
}
