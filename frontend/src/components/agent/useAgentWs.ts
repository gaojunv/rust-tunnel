import { useEffect } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { agentWsUrl } from '../../api/client';
import type { TFunction } from 'i18next';
import type { AgentWsEvent, SessionConfigOption, TodoItem } from '../../types';
import type { ChatItem } from './types';
import type { SlashCommand } from './SlashCommandPopup';
import { normalizeConfigOptions, restoreConfigValue } from './sessionConfig';
import {
  applyToolCallChunk,
  chunkKey,
  dropStreamPlaceholders,
  patchChildToolResult,
  STREAM_TOOL_ID_PREFIX,
  upsertToolCard,
} from './subagent';
import { nextLiveItemId } from './liveId';

const HEARTBEAT_TIMEOUT_MS = 75_000;
const WATCHDOG_INTERVAL_MS = 30_000;
const RESUME_STALE_MS = 30_000;

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

export interface UseAgentWsOptions {
  sessionId: string;
  tRef: React.MutableRefObject<TFunction>;
  setItems: React.Dispatch<React.SetStateAction<ChatItem[]>>;
  setDisconnected: React.Dispatch<React.SetStateAction<boolean>>;
  setConfigOptions: React.Dispatch<React.SetStateAction<SessionConfigOption[]>>;
  setSlashCommands: React.Dispatch<React.SetStateAction<SlashCommand[]>>;
  setTodos: React.Dispatch<React.SetStateAction<TodoItem[]>>;
  setContextUsage: React.Dispatch<React.SetStateAction<{ used?: number; size?: number } | null>>;
  setLastTurnDurationMs: React.Dispatch<React.SetStateAction<number | null>>;
  setApprovalMode: React.Dispatch<React.SetStateAction<string>>;
  runningRef: React.MutableRefObject<boolean>;
  pendingToolsRef: React.MutableRefObject<Set<string>>;
  lastFrameAtRef: React.MutableRefObject<number>;
  wsRef: React.MutableRefObject<WebSocket | null>;
  configRollbackRef: React.MutableRefObject<Record<string, { prev: string | boolean; opt: string | boolean }> | null>;
  planSeenThisTurnRef: React.MutableRefObject<boolean>;
  loadedRef: React.MutableRefObject<boolean>;
  reconcileRef: React.MutableRefObject<boolean>;
  partialLoadRef: React.MutableRefObject<boolean>;
  chunkBufRef: React.MutableRefObject<Map<string, string>>;
  chunkFlushTimerRef: React.MutableRefObject<ReturnType<typeof setTimeout> | null>;
  streamingIdxRef: React.MutableRefObject<number | null>;
  streamingKindRef: React.MutableRefObject<'assistant' | 'thought' | null>;
  subStreamRef: React.MutableRefObject<Map<string, { idx: number; kind: 'assistant' | 'thought' }>>;
  flushChunks: () => void;
  breakStream: () => void;
  breakSubStream: (parentToolId: string) => void;
  scheduleChunkFlush: () => void;
  armRunning: () => void;
  armRunningTimeout: () => void;
  stopRunning: () => void;
  clearRunningTimeout: () => void;
  expirePendingInteractions: () => void;
  reconcileConfigRollback: (serverOptions: SessionConfigOption[]) => void;
}

export function useAgentWs(opts: UseAgentWsOptions): void {
  const {
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
  } = opts;

  const queryClient = useQueryClient();

  useEffect(() => {
    let ws: WebSocket | null = null;
    let attempts = 0;
    let closedByCleanup = false;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    let watchdogTimer: ReturnType<typeof setInterval> | null = null;
    let needHistoryReload = false;
    let probeTimer: ReturnType<typeof setTimeout> | null = null;
    const pendingTools = pendingToolsRef.current;

    const connect = () => {
      ws = new WebSocket(agentWsUrl(sessionId));
      wsRef.current = ws;

      ws.onopen = () => {
        attempts = 0;
        setDisconnected(false);
        lastFrameAtRef.current = Date.now();
        if (needHistoryReload) {
          needHistoryReload = false;
          reconcileRef.current = true;
          loadedRef.current = false;
          void queryClient.invalidateQueries({ queryKey: ['agent-messages', sessionId] });
        }
      };

      ws.onmessage = (ev) => {
        lastFrameAtRef.current = Date.now();
        let msg: AgentWsEvent;
        try {
          msg = JSON.parse(ev.data) as AgentWsEvent;
        } catch {
          return;
        }
        if (runningRef.current && TURN_ACTIVITY_TYPES.has(msg.type)) {
          armRunningTimeout();
        }
        if (msg.type === 'heartbeat') {
          if (runningRef.current) armRunningTimeout();
        } else if (msg.type === 'assistant_chunk') {
          if (msg.content) {
            const parent = msg.parent_tool_call_id;
            const nextKind = msg.thought ? 'thought' : 'assistant';
            if (!parent) {
              if (streamingKindRef.current !== null && streamingKindRef.current !== nextKind) {
                flushChunks();
              }
              streamingKindRef.current = nextKind;
            }
            const key = chunkKey(parent, nextKind);
            chunkBufRef.current.set(key, (chunkBufRef.current.get(key) ?? '') + msg.content);
            scheduleChunkFlush();
          }
          if (msg.final) {
            flushChunks();
            if (msg.parent_tool_call_id) breakSubStream(msg.parent_tool_call_id);
            else breakStream();
          }
        } else if (msg.type === 'stream_reset') {
          const idx = streamingIdxRef.current;
          chunkBufRef.current = new Map();
          flushChunks();
          breakStream();
          setItems((prev) => {
            let next = prev;
            if (idx !== null) {
              const k = next[idx]?.kind;
              if (k === 'assistant' || k === 'thought') {
                next = next.filter((_, i) => i !== idx);
              }
            }
            return dropStreamPlaceholders(next);
          });
        } else if (msg.type === 'tool_call') {
          const parentToolId = msg.parent_tool_call_id;
          const isSubagentCard = msg.is_subagent === true;
          if (!parentToolId) {
            if (msg.id) pendingTools.add(msg.id);
            armRunning();
          }
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
            breakSubStream(parentToolId);
            setItems((prev) => {
              const parentIdx = prev.findIndex((it) => it.kind === 'tool' && it.toolId === parentToolId);
              if (parentIdx < 0) return upsertToolCard(prev, toolItem);
              const next = [...prev];
              const cleanedChildren = dropStreamPlaceholders(next[parentIdx].children ?? []);
              next[parentIdx] = { ...next[parentIdx], children: upsertToolCard(cleanedChildren, toolItem) };
              return next;
            });
          } else {
            breakStream();
            setItems((prev) => {
              const cleaned = prev.filter((it) => !(it.kind === 'tool' && it.toolId && it.toolId.startsWith(STREAM_TOOL_ID_PREFIX)));
              const orphanKids = toolItem.toolId ? cleaned.filter((it) => it.parentToolId === toolItem.toolId) : [];
              const filtered = orphanKids.length > 0 ? cleaned.filter((it) => it.parentToolId !== toolItem.toolId) : cleaned;
              return upsertToolCard(filtered, { ...toolItem, ...(orphanKids.length > 0 ? { children: orphanKids } : {}) });
            });
          }
        } else if (msg.type === 'tool_call_chunk') {
          armRunningTimeout();
          flushChunks();
          setItems((prev) => applyToolCallChunk(prev, msg));
        } else if (msg.type === 'tool_result') {
          const parentToolId = msg.parent_tool_call_id;
          if (parentToolId) {
            flushChunks();
            breakSubStream(parentToolId);
            setItems((prev) => {
              const parentIdx = prev.findIndex((it) => it.kind === 'tool' && it.toolId === parentToolId);
              if (parentIdx < 0) return patchChildToolResult(prev, msg);
              const next = [...prev];
              next[parentIdx] = { ...next[parentIdx], children: patchChildToolResult(next[parentIdx].children ?? [], msg) };
              return next;
            });
          } else {
            if (msg.id) pendingTools.delete(msg.id);
            setItems((prev) => {
              const next = [...prev];
              const patch = (i: number) => {
                const isNoop = (a: string | undefined) => {
                  const t = (a ?? '').trim();
                  return t === '' || t === '{}';
                };
                next[i] = {
                  ...next[i],
                  toolResult: msg.result,
                  toolStatus: msg.status ?? 'completed',
                  toolName: next[i].toolName ?? msg.name,
                  toolArgs: isNoop(next[i].toolArgs) && !isNoop(msg.args) ? msg.args : next[i].toolArgs ?? msg.args,
                  toolKind: next[i].toolKind ?? msg.tool_kind,
                  toolDiffs: next[i].toolDiffs ?? msg.diffs,
                  toolLocations: next[i].toolLocations ?? msg.locations,
                };
              };
              if (msg.id) {
                const byId = next.findIndex((it) => it.kind === 'tool' && it.toolId === msg.id);
                if (byId >= 0) {
                  patch(byId);
                  return next;
                }
              }
              if (msg.name) {
                for (let i = next.length - 1; i >= 0; i--) {
                  if (next[i].kind === 'tool' && next[i].toolName === msg.name && next[i].toolResult == null) {
                    patch(i);
                    return next;
                  }
                }
              }
              if (msg.id) {
                const pendingIdx: number[] = [];
                for (let i = 0; i < next.length; i++) if (next[i].kind === 'tool' && next[i].toolResult == null) pendingIdx.push(i);
                if (pendingIdx.length > 0) patch(pendingIdx[0]);
              }
              return next;
            });
          }
        } else if (msg.type === 'plan') {
          flushChunks();
          breakStream();
          const entries = msg.entries ?? [];
          if (!planSeenThisTurnRef.current) {
            planSeenThisTurnRef.current = true;
            setItems((prev) => [...prev, { kind: 'plan', content: '', planEntries: entries, id: nextLiveItemId() }]);
            return;
          }
          setItems((prev) => {
            for (let i = prev.length - 1; i >= 0; i--) if (prev[i].kind === 'plan') {
              const next = [...prev];
              next[i] = { ...next[i], planEntries: entries };
              return next;
            }
            return [...prev, { kind: 'plan', content: '', planEntries: entries, id: nextLiveItemId() }];
          });
        } else if (msg.type === 'usage') {
          setContextUsage({ used: msg.used, size: msg.size });
        } else if (msg.type === 'attachment') {
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
            breakSubStream(parentId);
            setItems((prev) => {
              const parentIdx = prev.findIndex((it) => it.kind === 'tool' && it.toolId === parentId);
              if (parentIdx < 0) return [...prev, card];
              const next = [...prev];
              next[parentIdx] = { ...next[parentIdx], children: [...(next[parentIdx].children ?? []), card] };
              return next;
            });
          } else setItems((prev) => [...prev, card]);
        } else if (msg.type === 'status') {
          flushChunks();
          breakStream();
          setItems((prev) => [...prev, { kind: 'system', systemTone: 'info', content: msg.message ?? '', id: nextLiveItemId() }]);
        } else if (msg.type === 'queued') {
          setItems((prev) => [...prev, { kind: 'system', systemTone: 'info', content: tRef.current('agent.messageQueued'), id: nextLiveItemId() }]);
        } else if (msg.type === 'stopped') {
          flushChunks();
          breakStream();
          stopRunning();
          planSeenThisTurnRef.current = false;
          expirePendingInteractions();
        } else if (msg.type === 'cancel_fallback') {
          flushChunks();
          breakStream();
          stopRunning();
          planSeenThisTurnRef.current = false;
          expirePendingInteractions();
          setItems((prev) => [...prev, { kind: 'system', systemTone: 'warning', content: tRef.current('agent.cancelFallback'), id: nextLiveItemId() }]);
        } else if (msg.type === 'done') {
          flushChunks();
          breakStream();
          stopRunning();
          if (typeof msg.duration_ms === 'number') setLastTurnDurationMs(msg.duration_ms);
          setItems((prev) => dropStreamPlaceholders(prev));
          planSeenThisTurnRef.current = false;
          expirePendingInteractions();
          if (partialLoadRef.current) {
            partialLoadRef.current = false;
            reconcileRef.current = true;
            loadedRef.current = false;
          }
          void queryClient.invalidateQueries({ queryKey: ['agent-messages', sessionId] });
          void queryClient.invalidateQueries({ queryKey: ['agent-sessions'] });
        } else if (msg.type === 'session_title') {
          void queryClient.invalidateQueries({ queryKey: ['agent-sessions'] });
        } else if (msg.type === 'session_state' || msg.type === 'config_option_update') {
          const serverOptions = normalizeConfigOptions(msg.options);
          setConfigOptions(serverOptions);
          if (msg.type === 'session_state' && Array.isArray(msg.available_commands)) setSlashCommands(msg.available_commands);
          reconcileConfigRollback(serverOptions);
        } else if (msg.type === 'current_mode_update') {
          setConfigOptions((prev) => prev.map((o) => (o.category === 'mode' && msg.mode_id ? { ...o, currentValue: msg.mode_id } : o)));
        } else if (msg.type === 'mode_updated') {
          if (msg.mode) setApprovalMode(msg.mode);
          void queryClient.invalidateQueries({ queryKey: ['agent-workspaces'] });
        } else if (msg.type === 'todo_update') {
          setTodos(msg.todos ?? []);
        } else if (msg.type === 'available_commands') {
          setSlashCommands(msg.commands ?? []);
        } else if (msg.type === 'approval_request') {
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
          if (configRollbackRef.current && msg.message?.startsWith('设置失败')) {
            const roll = configRollbackRef.current;
            configRollbackRef.current = null;
            setConfigOptions((cur) => cur.map((o) => (roll[o.id] !== undefined ? restoreConfigValue(o, roll[o.id]!.prev) : o)));
          }
          flushChunks();
          breakStream();
          setItems((prev) => [...prev, { kind: 'system', systemTone: 'error', content: msg.message ?? '', id: nextLiveItemId() }]);
          stopRunning();
          planSeenThisTurnRef.current = false;
          expirePendingInteractions();
        }
      };

      ws.onclose = () => {
        wsRef.current = null;
        configRollbackRef.current = null;
        if (closedByCleanup) return;
        if (runningRef.current) setItems((prev) => [...prev, { kind: 'system', systemTone: 'warning', content: tRef.current('agent.connectionInterrupted'), id: nextLiveItemId() }]);
        stopRunning();
        expirePendingInteractions();
        setDisconnected(true);
        needHistoryReload = true;
        const delay = Math.min(1000 * 2 ** attempts, 15000);
        attempts++;
        reconnectTimer = globalThis.setTimeout(connect, delay);
      };
      ws.onerror = () => {};
    };

    connect();
    watchdogTimer = globalThis.setInterval(() => {
      const w = wsRef.current;
      if (!w || w.readyState !== WebSocket.OPEN) return;
      if (Date.now() - lastFrameAtRef.current > HEARTBEAT_TIMEOUT_MS) w.close();
    }, WATCHDOG_INTERVAL_MS);

    const handleResume = () => {
      if (closedByCleanup) return;
      const w = wsRef.current;
      if (!w || w.readyState === WebSocket.CLOSED || w.readyState === WebSocket.CLOSING) {
        if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
        attempts = 0;
        connect();
        return;
      }
      if (w.readyState === WebSocket.CONNECTING) { w.close(); return; }
      if (probeTimer) return;
      if (Date.now() - lastFrameAtRef.current > RESUME_STALE_MS) { w.close(); return; }
      try { w.send(JSON.stringify({ type: 'ping' })); } catch { w.close(); return; }
      const probed = w;
      const probeSentAt = Date.now();
      probeTimer = globalThis.setTimeout(() => {
        probeTimer = null;
        if (wsRef.current === probed && lastFrameAtRef.current <= probeSentAt) probed.close();
      }, 2_000);
    };
    const onVisibility = () => { if (!document.hidden) handleResume(); };
    document.addEventListener('visibilitychange', onVisibility);
    globalThis.addEventListener('online', handleResume);
    return () => {
      closedByCleanup = true;
      document.removeEventListener('visibilitychange', onVisibility);
      globalThis.removeEventListener('online', handleResume);
      if (probeTimer) { clearTimeout(probeTimer); probeTimer = null; }
      if (watchdogTimer) { clearInterval(watchdogTimer); watchdogTimer = null; }
      if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
      if (ws) { ws.onclose = null; ws.onerror = null; ws.onopen = null; ws.close(); }
      wsRef.current = null;
      clearRunningTimeout();
      if (chunkFlushTimerRef.current) { clearTimeout(chunkFlushTimerRef.current); chunkFlushTimerRef.current = null; }
      chunkBufRef.current = new Map();
      pendingTools.clear();
    };
  }, [
    sessionId,
    queryClient,
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
  ]);
}
