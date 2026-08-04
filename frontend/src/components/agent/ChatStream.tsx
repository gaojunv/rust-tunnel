import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Button } from '@/components/ui/button';
import { Loader2, Wrench } from 'lucide-react';
import { agentWsUrl, listAgentMessages } from '../../api/client';
import type { AgentWsEvent } from '../../types';

interface ChatItem {
  kind: 'user' | 'assistant' | 'tool';
  content: string;
  toolName?: string;
  toolArgs?: string;
  toolResult?: string;
}

export default function ChatStream({ sessionId }: { sessionId: string }) {
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

  // WebSocket
  useEffect(() => {
    const ws = new WebSocket(agentWsUrl(sessionId));
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
        setItems((prev) => [...prev, { kind: 'tool', content: '', toolName: msg.name, toolArgs: msg.args }]);
      } else if (msg.type === 'tool_result') {
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
        setRunning(false);
        // 刷新共享的历史缓存，让 SidebarTabs（Git tab）拿到最新 tool 结果
        void queryClient.invalidateQueries({ queryKey: ['agent-messages', sessionId] });
      } else if (msg.type === 'error') {
        setItems((prev) => [...prev, { kind: 'assistant', content: `⚠️ ${msg.message}` }]);
        setRunning(false);
      }
    };
    ws.onclose = () => setRunning(false);
    ws.onerror = () => setRunning(false);
    return () => {
      ws.onclose = null;
      ws.onerror = null;
      ws.close();
    };
  }, [sessionId, queryClient]);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' });
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
    setRunning(true);
  };

  return (
    <div className="flex h-full flex-col">
      <div className="flex-1 space-y-3 overflow-y-auto p-4">
        {items.length === 0 && !running && (
          <p className="text-center text-sm text-muted-foreground">
            {t('agent.chatEmptyHint')}
          </p>
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
      <div className="border-t p-2">
        <div className="flex gap-2">
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
            className="flex-1 resize-none rounded-md border border-input bg-background px-3 py-2 text-sm"
            rows={2}
          />
          <Button onClick={send} disabled={running} className="self-end">
            {t('agent.send')}
          </Button>
        </div>
      </div>
    </div>
  );
}
