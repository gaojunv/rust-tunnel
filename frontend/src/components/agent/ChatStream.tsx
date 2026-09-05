import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQueryClient } from '@tanstack/react-query';
import { useVirtualizer } from '@tanstack/react-virtual';
import { Button } from '@/components/ui/button';
import { Loader2, SendHorizontal, Square } from 'lucide-react';
import { useMediaQuery } from '@/hooks/useMediaQuery';
import {
  getApiErrorMessage,
  updateAgentSessionModel,
} from '../../api/client';
import type { AgentSession, TodoItem } from '../../types';
import type { ChatItem } from './types';
import ApprovalCard from './ApprovalCard';
import ElicitationCard from './ElicitationCard';
import type { SlashCommand } from './SlashCommandPopup';
import MessageBubble from './MessageBubble';
import SessionSettingsMenu from './SessionSettingsMenu';
import SubagentTaskCard from './SubagentTaskCard';
import SubagentPanel from './SubagentPanel';
import SystemMessage from './SystemMessage';
import ModeEffortPicker from './ModeEffortPicker';
import ChatInput from './ChatInput';
import { optionValue, restoreConfigValue } from './sessionConfig';
import { collectSubagents } from './subagent';
import type { SessionConfigOption } from '../../types';
import { useStreamBuffer } from './useStreamBuffer';
export { STREAM_FLUSH_MS } from './useStreamBuffer';
import { useChatHistory } from './useChatHistory';
import { useAgentWs } from './useAgentWs';
import { nextLiveItemId } from './liveId';

const RUNNING_TIMEOUT_MS = 30 * 60 * 1000;

interface Props {
  sessionId: string;
  workspaceId: string;
  model: string;
  approvalMode?: string;
  onModelChange: (id: string) => void;
  active?: boolean;
  claudeTierModels?: string | null;
  agentType?: string | null;
}

export default function ChatStream({ sessionId, workspaceId, model, approvalMode: initialApprovalMode, onModelChange, active, claudeTierModels, agentType }: Props) {
  const { t } = useTranslation();
  const tRef = useRef(t);
  useEffect(() => {
    tRef.current = t;
  }, [t]);
  const queryClient = useQueryClient();
  const [items, setItems] = useState<ChatItem[]>([]);
  const isDesktop = useMediaQuery('(min-width: 768px)');
  const subagents = useMemo(() => collectSubagents(items), [items]);
  const [expandedSubagents, setExpandedSubagents] = useState<ReadonlySet<string>>(new Set());
  const [input, setInput] = useState('');
  const [running, setRunning] = useState(false);
  const [disconnected, setDisconnected] = useState(false);
  const [refs, setRefs] = useState<string[]>([]);
  const [slashCommands, setSlashCommands] = useState<SlashCommand[]>([]);
  const [configOptions, setConfigOptions] = useState<SessionConfigOption[]>([]);
  const [approvalMode, setApprovalMode] = useState(initialApprovalMode ?? 'safe');
  const [todos, setTodos] = useState<TodoItem[]>([]);
  const [contextUsage, setContextUsage] = useState<{ used?: number; size?: number } | null>(null);
  const [lastTurnDurationMs, setLastTurnDurationMs] = useState<number | null>(null);
  const configRollbackRef = useRef<Record<string, { prev: string | boolean; opt: string | boolean }> | null>(null);
  const wsRef = useRef<WebSocket | null>(null);
  const lastFrameAtRef = useRef(0);
  const bottomRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const earlierButtonRef = useRef<HTMLDivElement>(null);
  const lastButtonHeightRef = useRef(0);
  const pendingToolsRef = useRef<Set<string>>(new Set());
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const runningRef = useRef(false);
  const stickToBottomRef = useRef(true);
  const planSeenThisTurnRef = useRef(false);
  const respondedRequestRef = useRef<Set<string>>(new Set());
  const modelChangeSeqRef = useRef(0);
  const sentSinceOpenRef = useRef(false);

  const { chunkBufRef, chunkFlushTimerRef, streamingIdxRef, streamingKindRef, subStreamRef, flushChunks, breakStream, breakSubStream, scheduleChunkFlush } =
    useStreamBuffer({ setItems });

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

  const expirePendingInteractions = useCallback(() => {
    setItems((prev) =>
      prev.map((it) =>
        it.kind === 'approval' && it.approvalStatus === 'pending'
          ? { ...it, approvalStatus: 'expired' }
          : it.kind === 'elicitation' && it.elicitationStatus === 'pending'
            ? { ...it, elicitationStatus: 'cancelled' }
            : it,
      ),
    );
  }, []);

  const fireRunningTimeout = useCallback(() => {
    timeoutRef.current = null;
    setItems((prev) => [...prev, { kind: 'system', systemTone: 'warning', content: tRef.current('agent.responseTimeout'), id: nextLiveItemId() }]);
    expirePendingInteractions();
    planSeenThisTurnRef.current = false;
    stopRunning();
  }, [expirePendingInteractions, stopRunning]);

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

  const { hasMore, loadingEarlier, loadEarlier, loadedRef, partialLoadRef, reconcileRef } = useChatHistory({
    sessionId,
    items,
    setItems,
    runningRef,
    streamingIdxRef,
    scrollRef,
    earlierButtonRef,
    lastButtonHeightRef,
  });

  const reconcileConfigRollback = useCallback((serverOptions: SessionConfigOption[]) => {
    const roll = configRollbackRef.current;
    if (!roll) return;
    const next: typeof roll = {};
    for (const [id, entry] of Object.entries(roll)) {
      const serverVal = serverOptions.find((o) => o.id === id) && optionValue(serverOptions.find((o) => o.id === id)!);
      if (serverVal !== entry.opt) next[id] = entry;
    }
    configRollbackRef.current = Object.keys(next).length > 0 ? next : null;
  }, []);

  useEffect(() => {
    setConfigOptions([]);
  }, [sessionId]);

  useAgentWs({
    sessionId,
    tRef,
    setItems,
    setDisconnected,
    setConfigOptions,
    setSlashCommands,
    setTodos,
    setContextUsage,
    setLastTurnDurationMs,
    setApprovalMode,
    runningRef,
    pendingToolsRef,
    lastFrameAtRef,
    wsRef,
    configRollbackRef,
    planSeenThisTurnRef,
    loadedRef,
    reconcileRef,
    partialLoadRef,
    chunkBufRef,
    chunkFlushTimerRef,
    streamingIdxRef,
    streamingKindRef,
    subStreamRef,
    sentSinceOpenRef,
    flushChunks,
    breakStream,
    breakSubStream,
    scheduleChunkFlush,
    armRunning,
    armRunningTimeout,
    stopRunning,
    clearRunningTimeout,
    expirePendingInteractions,
    reconcileConfigRollback,
  });

  const canVirtualize = typeof ResizeObserver !== 'undefined';
  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 80,
    overscan: 8,
  });
  const virtualItems = canVirtualize ? virtualizer.getVirtualItems() : null;

  const toggleExpandedSubagent = useCallback((toolId: string) => {
    setExpandedSubagents((prev) => {
      const next = new Set(prev);
      if (next.has(toolId)) next.delete(toolId);
      else next.add(toolId);
      return next;
    });
  }, []);

  const itemsRefForSelect = useRef<ChatItem[]>([]);
  useEffect(() => {
    itemsRefForSelect.current = items;
  }, [items]);
  const handleSelectSubagent = useCallback(
    (index: number) => {
      const item = itemsRefForSelect.current[index];
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

  const totalSize = virtualizer.getTotalSize();
  useEffect(() => {
    if (stickToBottomRef.current) {
      bottomRef.current?.scrollIntoView?.({ behavior: 'auto' });
    }
  }, [items, totalSize]);

  const prevActiveRef = useRef(active);
  useEffect(() => {
    const wasInactive = prevActiveRef.current !== true;
    prevActiveRef.current = active;
    if (active && wasInactive && stickToBottomRef.current) {
      virtualizer.measure();
      bottomRef.current?.scrollIntoView?.({ behavior: 'auto' });
    }
  }, [active, virtualizer]);

  const hasPendingInteraction = items.some(
    (it) => (it.kind === 'approval' && it.approvalStatus === 'pending') || (it.kind === 'elicitation' && it.elicitationStatus === 'pending'),
  );

  const respondApproval = (id: string, approved: boolean, remember: boolean, optionId?: string) => {
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      setItems((prev) => [...prev, { kind: 'system', systemTone: 'error', content: t('agent.connectionLost'), id: nextLiveItemId() }]);
      return;
    }
    if (respondedRequestRef.current.has(id)) return;
    respondedRequestRef.current.add(id);
    const payload: Record<string, unknown> = {
      type: 'approval_response',
      request_id: id,
      approved,
      remember: remember ? 'session' : 'none',
    };
    if (optionId) payload.option_id = optionId;
    ws.send(JSON.stringify(payload));
    setItems((prev) => prev.map((it) => (it.kind === 'approval' && it.approvalId === id ? { ...it, approvalStatus: approved ? 'approved' : 'denied' } : it)));
  };

  const respondElicitation = (id: string, action: 'accept' | 'decline' | 'cancel', content?: Record<string, unknown>) => {
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      setItems((prev) => [...prev, { kind: 'system', systemTone: 'error', content: t('agent.connectionLost'), id: nextLiveItemId() }]);
      return;
    }
    if (respondedRequestRef.current.has(id)) return;
    respondedRequestRef.current.add(id);
    const payload: Record<string, unknown> = { type: 'elicitation_response', request_id: id, action };
    if (content && Object.keys(content).length > 0) payload.content = content;
    ws.send(JSON.stringify(payload));
    const status = action === 'accept' ? 'accepted' : action === 'decline' ? 'declined' : 'cancelled';
    setItems((prev) => prev.map((it) => (it.kind === 'elicitation' && it.elicitationId === id ? { ...it, elicitationStatus: status } : it)));
  };

  const send = useCallback(() => {
    const text = input.trim();
    if (!text || hasPendingInteraction) return;
    const ws = wsRef.current;
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
    sentSinceOpenRef.current = true;
    setItems((prev) => [...prev, { kind: 'user', content: text, id: nextLiveItemId() }]);
    setInput('');
    setRefs([]);
    armRunning();
  }, [input, hasPendingInteraction, refs, armRunning, t]);

  const stop = useCallback(() => {
    const ws = wsRef.current;
    if (ws && ws.readyState === WebSocket.OPEN) {
      try {
        ws.send(JSON.stringify({ type: 'cancel' }));
      } catch {
        /* ignore */
      }
    }
    flushChunks();
    breakStream();
    stopRunning();
    expirePendingInteractions();
    setItems((prev) => [...prev, { kind: 'system', systemTone: 'stopped', content: t('agent.stopped'), id: nextLiveItemId() }]);
  }, [flushChunks, breakStream, stopRunning, expirePendingInteractions, t]);

  const triggerCompact = useCallback(() => {
    const cmd = slashCommands.find((c) => c.name.toLowerCase().includes('compact'));
    const ws = wsRef.current;
    if (!cmd || !ws || ws.readyState !== WebSocket.OPEN) return;
    const text = `/${cmd.name}`;
    try {
      ws.send(JSON.stringify({ type: 'user_message', content: text, refs: [] }));
    } catch {
      setItems((prev) => [...prev, { kind: 'system', systemTone: 'error', content: t('agent.connectionLost'), id: nextLiveItemId() }]);
      return;
    }
    sentSinceOpenRef.current = true;
    setItems((prev) => [...prev, { kind: 'user', content: text, id: nextLiveItemId() }]);
    armRunning();
  }, [slashCommands, armRunning, t]);

  const handleModelChange = (id: string) => {
    const prev = model;
    const seq = ++modelChangeSeqRef.current;
    onModelChange(id);
    void updateAgentSessionModel(sessionId, id)
      .then(() => {
        void queryClient.invalidateQueries({ queryKey: ['agent-sessions'] });
      })
      .catch((err: unknown) => {
        if (seq !== modelChangeSeqRef.current) return;
        onModelChange(prev);
        setItems((prevItems) => [...prevItems, { kind: 'system', systemTone: 'error', content: `${t('agent.modelUpdateFailed')}: ${getApiErrorMessage(err)}`, id: nextLiveItemId() }]);
      });
  };

  const sendConfigOption = (configId: string, value: string) => {
    const target = configOptions.find((o) => o.id === configId);
    if (target) {
      const optVal: string | boolean = target.type === 'boolean' ? value === 'true' : value;
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
      const entry = configRollbackRef.current?.[configId];
      configRollbackRef.current = null;
      if (entry) {
        setConfigOptions((cur) => cur.map((o) => (o.id === configId ? restoreConfigValue(o, entry.prev) : o)));
      }
      return;
    }
    try {
      ws.send(JSON.stringify({ type: 'set_config_option', config_id: configId, value }));
    } catch {
      const entry = configRollbackRef.current?.[configId];
      configRollbackRef.current = null;
      if (entry) {
        setConfigOptions((cur) => cur.map((o) => (o.id === configId ? restoreConfigValue(o, entry.prev) : o)));
      }
      setItems((prevItems) => [...prevItems, { kind: 'system', systemTone: 'error', content: t('agent.connectionLost'), id: nextLiveItemId() }]);
    }
  };

  const modeOption = configOptions.find((o) => o.category === 'mode');
  const effortOption = configOptions.find((o) => o.category === 'thought_level');
  const menuOptions = configOptions.filter((o) => o.category !== 'mode' && o.category !== 'thought_level');

  const sendSetMode = useCallback(
    (newMode: string) => {
      const ws = wsRef.current;
      if (!ws || ws.readyState !== WebSocket.OPEN) return;
      try {
        ws.send(JSON.stringify({ type: 'set_mode', mode: newMode }));
        setApprovalMode(newMode);
      } catch {
        /* ignore */
      }
    },
    [],
  );

  useEffect(() => {
    if (initialApprovalMode) setApprovalMode(initialApprovalMode);
  }, [initialApprovalMode]);

  useEffect(() => {
    const sessions = queryClient.getQueryData<AgentSession[]>(['agent-sessions', workspaceId]);
    const s = sessions?.find((x) => x.id === sessionId);
    setContextUsage(s && (s.context_used != null || s.context_size != null) ? { used: s.context_used ?? undefined, size: s.context_size ?? undefined } : null);
    setLastTurnDurationMs(null);
  }, [queryClient, workspaceId, sessionId]);

  const sessionRecord = queryClient.getQueryData<AgentSession[]>(['agent-sessions', workspaceId])?.find((x) => x.id === sessionId);

  const renderItem = (it: ChatItem, i: number) => {
    const isStreaming = streamingIdxRef.current === i && (it.kind === 'assistant' || it.kind === 'thought');
    if (it.kind === 'system') {
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
          open={it.toolId ? expandedSubagents.has(it.toolId) : undefined}
          onToggle={it.toolId ? () => toggleExpandedSubagent(it.toolId!) : undefined}
        />
      );
    }
    return <MessageBubble key={it.kind === 'tool' && it.toolId ? it.toolId : (it.id ?? i)} item={it} streaming={isStreaming} />;
  };

  return (
    <div className="relative flex h-full">
      <div className="relative flex min-w-0 flex-1 flex-col">
        {!isDesktop && subagents.length > 0 && <SubagentPanel variant="top" summaries={subagents} onSelect={handleSelectSubagent} expandedIds={expandedSubagents} />}
        <div
          ref={scrollRef}
          data-testid="chat-scroll-container"
          className="flex-1 overflow-y-auto px-3 pt-3 md:px-5 md:pt-4 dark:text-foreground/85"
          onScroll={(e) => {
            const el = e.currentTarget;
            stickToBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
          }}
        >
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
              {items.length === 0 && !running && <p className="text-center text-sm text-muted-foreground">{t('agent.chatEmptyHint')}</p>}
              {virtualItems ? (
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
                <div className="space-y-3 md:space-y-4">{items.map((it, i) => renderItem(it, i))}</div>
              )}
              <div ref={bottomRef} />
            </div>

            <div className="sticky bottom-0 z-20 -mx-3 px-3 pb-3 pt-1.5 md:-mx-5 md:px-5 md:pb-5 md:pt-2">
              <div className="pointer-events-none absolute inset-0 bg-[linear-gradient(to_bottom,hsl(var(--card)/0),hsl(var(--card))_calc(100%_-_0.75rem))] md:bg-[linear-gradient(to_bottom,hsl(var(--card)/0),hsl(var(--card))_calc(100%_-_1.25rem))]" />
              <div className="relative mx-auto w-full max-w-3xl">
                {running && <span role="status" aria-label={t('agent.running')} className="sr-only" />}
                {disconnected && (
                  <div className="mb-1 flex items-center gap-1.5 rounded-md bg-destructive/10 px-2.5 py-1.5 text-xs text-destructive md:mb-1.5">
                    <Loader2 className="h-3 w-3 animate-spin" />
                    {t('agent.reconnecting')}
                  </div>
                )}
                {todos.length > 0 && (
                  <div className="mb-2 rounded-xl border border-border/60 bg-muted/30 px-3 py-2">
                    <div className="mb-1 text-xs font-medium text-muted-foreground">{t('agent.tasks')}</div>
                    <ul className="space-y-0.5">
                      {todos.map((todoItem, i) => (
                        <li key={i} className="flex items-start gap-1.5 text-xs">
                          <span className="mt-0.5 shrink-0">{todoItem.status === 'completed' ? '✅' : todoItem.status === 'in_progress' ? '🔄' : '⬜'}</span>
                          <span className={todoItem.status === 'completed' ? 'text-muted-foreground line-through' : todoItem.status === 'in_progress' ? 'font-medium' : ''}>
                            {todoItem.activeForm && todoItem.status === 'in_progress' ? `${todoItem.activeForm}: ` : ''}
                            {todoItem.content}
                          </span>
                        </li>
                      ))}
                    </ul>
                  </div>
                )}
                {lastTurnDurationMs != null && !running && (
                  <div className="mb-2 px-1 text-[10px] tabular-nums text-muted-foreground" data-testid="turn-duration">
                    {t('agent.turnDuration')} {lastTurnDurationMs < 1000 ? `${lastTurnDurationMs}ms` : `${(lastTurnDurationMs / 1000).toFixed(1)}s`}
                  </div>
                )}
                <div className={`relative mx-2 rounded-2xl border bg-background shadow-md focus-within:ring-1 focus-within:ring-ring md:mx-3 ${running ? 'agent-input-running' : 'border-input'}`}>
                  <ChatInput
                    input={input}
                    onInputChange={setInput}
                    refs={refs}
                    setRefs={setRefs}
                    workspaceId={workspaceId}
                    slashCommands={slashCommands}
                    textareaRef={textareaRef}
                    onSend={send}
                    placeholder={t('agent.inputPlaceholder')}
                  />
                  <div className="flex flex-wrap items-center justify-between gap-1 border-t border-border/60 px-1.5 pb-1.5 pt-1 md:px-2">
                    <div className="flex items-center gap-0.5">
                      <SessionSettingsMenu
                        model={model}
                        onModelChange={handleModelChange}
                        configOptions={menuOptions}
                        onConfigChange={sendConfigOption}
                        sessionId={sessionId}
                        roleId={sessionRecord?.role_id}
                        claudeTierModels={claudeTierModels}
                        agentType={agentType}
                        configState={sessionRecord?.config_state}
                      />
                      {contextUsage?.size != null && contextUsage.size > 0 &&
                        (() => {
                          const used = contextUsage.used ?? 0;
                          const size = contextUsage.size;
                          const pct = Math.min(100, Math.round((used / size) * 100));
                          const tone = pct > 95 ? 'text-destructive' : pct > 80 ? 'text-yellow-500' : 'text-primary/70';
                          const compactCmd = slashCommands.find((c) => c.name.toLowerCase().includes('compact'));
                          const clickable = pct > 50 && !!compactCmd && !running && !hasPendingInteraction;
                          const R = 7;
                          const C = 2 * Math.PI * R;
                          const tip = t('agent.contextUsageTooltip', { used, size });
                          return (
                            <button
                              type="button"
                              data-testid="context-usage-ring"
                              aria-label={tip}
                              aria-disabled={!clickable}
                              title={clickable ? `${tip} · ${t('agent.contextCompactHint')}` : tip}
                              onClick={() => {
                                if (clickable) triggerCompact();
                              }}
                              className={`flex h-7 items-center rounded-full px-1 text-muted-foreground transition-colors ${clickable ? 'hover:bg-accent hover:text-foreground' : 'cursor-default'}`}
                            >
                              <svg viewBox="0 0 18 18" className={`h-[18px] w-[18px] -rotate-90 ${tone}`} aria-hidden>
                                <circle cx="9" cy="9" r={R} fill="none" strokeWidth="2.5" className="stroke-muted" />
                                <circle
                                  cx="9"
                                  cy="9"
                                  r={R}
                                  fill="none"
                                  strokeWidth="2.5"
                                  strokeLinecap="round"
                                  className="stroke-current transition-[stroke-dashoffset]"
                                  strokeDasharray={C}
                                  strokeDashoffset={C * (1 - pct / 100)}
                                />
                              </svg>
                            </button>
                          );
                        })()}
                    </div>
                    <div className="flex items-center gap-0.5">
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
                      <ModeEffortPicker modeOption={modeOption} effortOption={effortOption} onChange={sendConfigOption} placeholder={configOptions.length > 0} />
                      {running && !input.trim() ? (
                        <Button onClick={stop} size="sm" variant="ghost" aria-label={t('agent.stop')} className="h-8 w-8 rounded-full p-0 text-destructive hover:text-destructive">
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
      {isDesktop && subagents.length > 0 && <SubagentPanel variant="sidebar" summaries={subagents} onSelect={handleSelectSubagent} expandedIds={expandedSubagents} />}
    </div>
  );
}
