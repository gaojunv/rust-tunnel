import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Button } from '@/components/ui/button';
import { Loader2, SendHorizontal, Wrench } from 'lucide-react';
import { agentWsUrl, listAgentMessages, updateAgentSessionModel } from '../../api/client';
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

  // 历史消息（与 SidebarTabs 共享 queryKey，invalidate 后 Git 面板自动刷新）
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
    for (const m of history) {
      if (m.role === 'tool' && m.tool_calls) {
        try {
          for (const t of JSON.parse(m.tool_calls)) {
            loaded.push({ kind: 'tool', content: '', toolName: t.name, toolArgs: t.args, toolResult: t.result });
          }
        } catch {
          /* ignore malformed tool_calls */
        }
      } else if (m.content) {
        loaded.push({ kind: m.role === 'user' ? 'user' : 'assistant', content: m.content });
      }
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
      if (msg.type === 'assistant_chunk' && msg.content) {
        setItems((prev) => [...prev, { kind: 'assistant', content: msg.content! }]);
      } else if (msg.type === 'tool_call') {
        if (msg.id) pendingTools.add(msg.id);
        // 服务端进入工具执行 → 显示 Running（对无前置 send 的乱序帧同样成立）
        armRunning();
        setItems((prev) => [...prev, { kind: 'tool', content: '', toolName: msg.name, toolArgs: msg.args }]);
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
      } else if (msg.type === 'done') {
        // 严格终态：工具全部回齐才解除 Running（防御乱序帧）
        if (pendingTools.size === 0) {
          stopRunning();
          // 刷新共享的历史缓存，让 SidebarTabs（Git tab）拿到最新 tool 结果
          void queryClient.invalidateQueries({ queryKey: ['agent-messages', sessionId] });
        }
      } else if (msg.type === 'error') {
        setItems((prev) => [...prev, { kind: 'assistant', content: `⚠️ ${msg.message}` }]);
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
    onModelChange(id);
    void updateAgentSessionModel(sessionId, id).catch(() => {
      /* 失败回滚由 AgentPage invalidate 处理，此处静默 */
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
