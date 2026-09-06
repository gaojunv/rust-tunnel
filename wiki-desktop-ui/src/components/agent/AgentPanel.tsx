/**
 * Agent 面板 —— M2 完整版 + M3 引导联动
 * 审批弹窗 + MessageStream + ThreadList + vault 一致性闭环
 * M3：未启用/认证缺失/exited 摘要引导，onOpenSettings 联动
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Bot, Send, Square, AlertTriangle, List as ListIcon, MessageSquare, Settings } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  getStatus,
  start,
  turnStart,
  turnSteer,
  turnInterrupt,
  threadStart,
  agentRespond,
  onAgentNotification,
  onAgentStatus,
  onServerRequest,
  getSettings,
  getAuthStatus,
} from "@/lib/agent/client";
import type { AgentSettingsDto, AgentStatusDto } from "@/lib/agent/client";
import { createInitialView, reduceAgentEvent, reduceServerRequest, resolveServerRequest } from "@/lib/agent/codec";
import type { AgentThreadView, ApprovalRequestView } from "@/lib/agent/codec";
import { addThread, listThreads, updateThreadModel } from "@/lib/agent/store";
import type { ServerNotification } from "@/lib/agent/types";
import type { Thread } from "@/lib/agent/types/v2/Thread";
import { ApprovalDialog } from "@/components/agent/ApprovalDialog";
import { MessageStream } from "@/components/agent/MessageStream";
import { ThreadList } from "@/components/agent/ThreadList";
import { createAllowlist, allowlistHas, allowlistAdd } from "@/lib/agent/allowlist";
import type { Allowlist } from "@/lib/agent/allowlist";

function isTauriEnv(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

function getAuthMissingHint(settings: AgentSettingsDto | null, chatAuthed: boolean | null): string | null {
  if (!settings) return null;
  const mode = String(settings.authMode ?? "gateway");
  if (mode === "gateway") {
    if (!String(settings.gatewayApiKey ?? "").trim()) return "网关认证缺少 API Key，请在设置中填写后重试";
    return null;
  }
  if (mode === "openai-key") {
    if (!String(settings.openaiApiKey ?? "").trim()) return "OpenAI 认证缺少 API Key，请在设置中填写后重试";
    return null;
  }
  if (mode === "chatgpt") {
    if (chatAuthed === false) return "ChatGPT 未登录，请在设置中完成登录后重试";
    if (chatAuthed === null) return null;
    return null;
  }
  return null;
}

type Props = {
  onInsertToNote: (text: string) => void;
  vaultRoot: string | null;
  onVaultChanged: () => void;
  flushSave: () => Promise<void>;
  onOpenSettings?: () => void;
};

type PanelMode = "threadList" | "conversation";

export function AgentPanel({ onInsertToNote, vaultRoot, onVaultChanged, flushSave, onOpenSettings }: Props) {
  const isDesktop = isTauriEnv();

  const [status, setStatus] = useState<AgentStatusDto | null>(null);
  const [statusLoading, setStatusLoading] = useState(true);
  const [statusError, setStatusError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  const [view, setView] = useState<AgentThreadView>(() => createInitialView());
  const [input, setInput] = useState("");
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [queue, setQueue] = useState<ApprovalRequestView[]>([]);
  const [allowlist] = useState<Allowlist>(() => createAllowlist());
  const [allowlistVersion, setAllowlistVersion] = useState(0);
  const [panelMode, setPanelMode] = useState<PanelMode>("conversation");
  const [selectedModel, setSelectedModel] = useState<string | null>(null);
  const [agentSettings, setAgentSettings] = useState<AgentSettingsDto | null>(null);
  const [chatAuthed, setChatAuthed] = useState<boolean | null>(null);

  const fileChangeInTurnRef = useRef(false);
  const allowlistRef = useRef(allowlist);
  useEffect(() => {
    allowlistRef.current = allowlist;
  }, [allowlist]);

  const listRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef(view);
  useEffect(() => {
    viewRef.current = view;
  }, [view]);

  useEffect(() => {
    if (!vaultRoot || !view.threadId) return;
    try {
      const rec = listThreads(vaultRoot).find((r) => r.threadId === view.threadId);
      if (rec?.model) setSelectedModel(rec.model);
    } catch {
      // ignore
    }
  }, [vaultRoot, view.threadId]);

  const bumpAllowlist = useCallback(() => setAllowlistVersion((n) => n + 1), []);
  void allowlistVersion;

  useEffect(() => {
    if (!isDesktop) {
      setStatusLoading(false);
      return;
    }
    let cancelled = false;
    getStatus()
      .then((s) => {
        if (!cancelled) setStatus(s);
      })
      .catch((e: unknown) => {
        if (!cancelled) setStatusError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (!cancelled) setStatusLoading(false);
      });
    // M3：加载 settings 以驱动未启用/认证缺失引导
    getSettings()
      .then((s) => {
        if (!cancelled) setAgentSettings(s);
      })
      .catch(() => {
        // ignore
      });
    return () => {
      cancelled = true;
    };
  }, [isDesktop]);

  // 失焦后重新进入面板时刷新 settings（设置页保存后）
  useEffect(() => {
    if (!isDesktop) return;
    const onFocus = () => {
      getSettings()
        .then((s) => setAgentSettings(s))
        .catch(() => {});
    };
    const onVisibility = () => {
      if (document.visibilityState === "visible") onFocus();
    };
    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [isDesktop]);

  useEffect(() => {
    if (!isDesktop) return;
    let unlistenStatus: (() => void) | null = null;
    let unlistenNotif: (() => void) | null = null;
    let unlistenReq: (() => void) | null = null;
    let cancelled = false;

    void onAgentStatus((s) => {
      if (cancelled) return;
      setStatus(s);
      if (s.phase === "exited" || s.phase === "stopped") {
        setView((prev) => ({ ...prev, status: "idle", activeTurnId: null }));
        setQueue([]);
      }
    }).then((fn) => {
      unlistenStatus = fn;
    });

    void onAgentNotification((n: ServerNotification) => {
      if (cancelled) return;
      const method = (n as unknown as { method: string }).method;
      setView((prev) => reduceAgentEvent(prev, n));
      const params = (n as unknown as { params: unknown }).params as Record<string, unknown> | null | undefined;
      if (method === "item/started") {
        const item = (params as Record<string, unknown>)?.["item"] as Record<string, unknown> | undefined;
        if (item?.["type"] === "fileChange") fileChangeInTurnRef.current = true;
      }
      if (method === "fs/changed") {
        fileChangeInTurnRef.current = true;
      }
      if (method === "turn/started") {
        fileChangeInTurnRef.current = false;
      }
      if (method === "turn/completed") {
        const hadFileChange = fileChangeInTurnRef.current;
        const hasFileChangeItem = viewRef.current.items.some((it) => it.kind === "fileChange");
        if (hadFileChange || hasFileChangeItem) {
          onVaultChanged();
        }
        fileChangeInTurnRef.current = false;
      }
      requestAnimationFrame(() => {
        if (listRef.current) listRef.current.scrollTop = listRef.current.scrollHeight;
      });
    }).then((fn) => {
      unlistenNotif = fn;
    });

    void onServerRequest((req: { id: unknown; method: string; params: unknown }) => {
      if (cancelled) return;
      if (allowlistHas(allowlistRef.current, req.method, req.params)) {
        const sendAuto = () => {
          switch (req.method) {
            case "item/commandExecution/requestApproval":
              return agentRespond(req.id, { decision: "acceptForSession" as const });
            case "item/fileChange/requestApproval":
              return agentRespond(req.id, { decision: "acceptForSession" as const });
            case "item/permissions/requestApproval": {
              const p = (req.params ?? {}) as Record<string, unknown>;
              const requested = (p["permissions"] ?? { network: null, fileSystem: null }) as unknown;
              return agentRespond(req.id, {
                permissions: requested as Record<string, unknown>,
                scope: "session" as const,
              });
            }
            case "execCommandApproval":
              return agentRespond(req.id, { decision: "approved_for_session" as const });
            case "applyPatchApproval":
              return agentRespond(req.id, { decision: "approved_for_session" as const });
            default:
              return agentRespond(req.id, { decision: "acceptForSession" as const });
          }
        };
        void sendAuto().catch(() => {});
        return;
      }
      setQueue((prev) => reduceServerRequest(prev, { id: req.id, method: req.method, params: req.params }));
    }).then((fn) => {
      unlistenReq = fn;
    });

    return () => {
      cancelled = true;
      unlistenStatus?.();
      unlistenNotif?.();
      unlistenReq?.();
    };
  }, [isDesktop, onVaultChanged]);

  const handleApprovalRespond = useCallback(
    (id: unknown, result: unknown, errorVal?: unknown) => {
      const head = queue[0];
      if (head && String(head.id) === String(id)) {
        const res = (result ?? {}) as Record<string, unknown>;
        const decision = res["decision"] as string | undefined;
        const scope = res["scope"] as string | undefined;
        const isSessionRemember =
          decision === "acceptForSession" || decision === "approved_for_session" || scope === "session";
        if (isSessionRemember) {
          allowlistAdd(allowlistRef.current, head.method, head.params);
          bumpAllowlist();
        }
      }
      setQueue((prev) => resolveServerRequest(prev, id));
      void agentRespond(id, result, errorVal).catch(() => {});
    },
    [queue, bumpAllowlist],
  );

  const handleStart = useCallback(async () => {
    if (!isDesktop) return;
    // 未启用引导不拦截启动（用户可能临时启用）；但认证缺失先提示
    const hint = getAuthMissingHint(agentSettings, chatAuthed);
    if (hint) {
      setStatusError(hint);
      return;
    }
    setStarting(true);
    setStatusError(null);
    try {
      const s = await start();
      setStatus(s);
      // 启动后若为 chatgpt 模式，主动探测一次认证状态以更新引导文案
      const mode = String(agentSettings?.authMode ?? "");
      if (mode === "chatgpt") {
        try {
          const auth = await getAuthStatus({ includeToken: false, refreshToken: false });
          setChatAuthed(auth.requiresOpenaiAuth === false);
        } catch {
          // ignore
        }
      }
    } catch (e: unknown) {
      setStatusError(e instanceof Error ? e.message : String(e));
    } finally {
      setStarting(false);
    }
  }, [isDesktop, agentSettings, chatAuthed]);

  // settings 变化且为 chatgpt 时，延迟探测 auth 状态（避免频繁）
  useEffect(() => {
    if (!isDesktop) return;
    if (String(agentSettings?.authMode ?? "") !== "chatgpt") {
      setChatAuthed(null);
      return;
    }
    // 仅当 running 时才有意义；stopped 时也可探测，但开销可接受
    let cancelled = false;
    getAuthStatus({ includeToken: false, refreshToken: false })
      .then((r) => {
        if (cancelled) return;
        setChatAuthed(r.requiresOpenaiAuth === false);
      })
      .catch(() => {
        if (!cancelled) setChatAuthed(null);
      });
    return () => {
      cancelled = true;
    };
  }, [isDesktop, agentSettings?.authMode]);

  const handleSelectThread = useCallback(
    (threadId: string, hydrated: { thread: Thread } | null) => {
      if (hydrated?.thread) {
        const thread = hydrated.thread;
        try {
          const turns = (thread as unknown as { turns?: unknown[] }).turns;
          if (Array.isArray(turns) && turns.length > 0) {
            setView((prev) => ({
              ...prev,
              threadId,
              items:
                prev.threadId === threadId
                  ? prev.items
                  : [
                      {
                        kind: "unknown" as const,
                        id: `resume-note-${Date.now()}`,
                        raw: { note: "已恢复会话，历史消息未回填，可直接续聊", threadId },
                      },
                    ],
              activeTurnId: null,
              status: "idle",
            }));
          } else {
            setView((prev) => ({ ...prev, threadId, activeTurnId: null, status: "idle" }));
          }
        } catch {
          setView((prev) => ({ ...prev, threadId, activeTurnId: null, status: "idle" }));
        }
      } else {
        setView((prev) => ({ ...prev, threadId, activeTurnId: null, status: "idle" }));
      }
      setPanelMode("conversation");
      if (vaultRoot && selectedModel) {
        try {
          updateThreadModel(vaultRoot, threadId, selectedModel);
        } catch {
          // ignore
        }
      }
    },
    [vaultRoot, selectedModel],
  );

  const handleNewThread = useCallback(() => {
    setView((prev) => ({ ...prev, threadId: "", items: [], activeTurnId: null, status: "idle" }));
    setPanelMode("conversation");
  }, []);

  const handleArchived = useCallback((threadId: string) => {
    if (viewRef.current.threadId === threadId) {
      setView((prev) => ({ ...prev, threadId: "", items: [], activeTurnId: null, status: "idle" }));
    }
  }, []);

  const handleModelChange = useCallback(
    (next: string | null) => {
      setSelectedModel(next);
      const tid = viewRef.current.threadId;
      if (vaultRoot && tid) {
        try {
          updateThreadModel(vaultRoot, tid, next);
        } catch {
          // ignore
        }
      }
    },
    [vaultRoot],
  );

  const handleSend = useCallback(async () => {
    const text = input.trim();
    if (!text || sending) return;
    const hint = getAuthMissingHint(agentSettings, chatAuthed);
    if (hint) {
      setError(hint);
      return;
    }
    setError(null);
    try {
      await flushSave();
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      console.warn("[agent] flushSave 失败：", msg);
    }

    setSending(true);
    setView((prev) => {
      const exists = prev.items.some((it) => it.kind === "userMessage" && it.text === text);
      if (exists) return prev;
      return {
        ...prev,
        items: [...prev.items, { kind: "userMessage" as const, id: `local-${Date.now()}`, text }],
      };
    });
    setInput("");

    try {
      let threadId = viewRef.current.threadId;
      const modelForStart = selectedModel ?? null;
      if (!threadId) {
        const res = await threadStart({ ...(modelForStart ? { model: modelForStart } : {}) });
        threadId = res.thread.id;
        setView((prev) => ({ ...prev, threadId }));
        if (vaultRoot) {
          try {
            const title = text.slice(0, 32) || "新会话";
            addThread(vaultRoot, {
              threadId,
              title,
              createdAt: Math.floor(Date.now() / 1000),
              model: (res.model as string | null) ?? modelForStart,
            });
          } catch {
            // 忽略持久化异常
          }
        }
      } else if (modelForStart && vaultRoot) {
        try {
          updateThreadModel(vaultRoot, threadId, modelForStart);
        } catch {
          // ignore
        }
      }
      const cur = viewRef.current;
      if (cur.activeTurnId) {
        await turnSteer({
          threadId,
          expectedTurnId: cur.activeTurnId,
          input: [{ type: "text", text, text_elements: [] }],
        });
      } else {
        await turnStart({
          threadId,
          input: [{ type: "text", text, text_elements: [] }],
          ...(modelForStart ? { model: modelForStart } : {}),
        });
      }
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSending(false);
      requestAnimationFrame(() => {
        if (listRef.current) listRef.current.scrollTop = listRef.current.scrollHeight;
      });
    }
  }, [input, sending, flushSave, vaultRoot, selectedModel, agentSettings, chatAuthed]);

  const handleInterrupt = useCallback(async () => {
    const cur = viewRef.current;
    if (!cur.threadId || !cur.activeTurnId) return;
    try {
      await turnInterrupt({ threadId: cur.threadId, turnId: cur.activeTurnId });
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void handleSend();
    }
    if (e.key === "Escape" && view.activeTurnId) {
      e.preventDefault();
      void handleInterrupt();
    }
  };

  useEffect(() => {
    const h = (e: KeyboardEvent) => {
      if (e.key === "Escape" && view.activeTurnId) {
        e.preventDefault();
        void handleInterrupt();
      }
    };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [view.activeTurnId, handleInterrupt]);

  if (!isDesktop) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 p-6 text-center">
        <Bot className="size-8 text-muted-foreground/60" />
        <p className="text-sm font-medium">Agent 仅在桌面端可用</p>
        <p className="max-w-[22rem] text-xs text-muted-foreground">
          请使用 Tauri 桌面应用打开此 vault，浏览器预览模式不支持 Codex Agent。
        </p>
      </div>
    );
  }

  if (statusLoading) {
    return <div className="p-4 text-xs text-muted-foreground">加载 Agent 状态…</div>;
  }

  const phase = status?.phase ?? "stopped";
  const isRunning = phase === "running";
  const isExited = phase === "exited";
  const isStarting = phase === "starting" || starting;

  if (!isRunning) {
    const notEnabled = agentSettings != null && agentSettings.enabled === false;
    const authHint = getAuthMissingHint(agentSettings, chatAuthed);
    // 优先展示未启用引导，其次认证缺失
    const preStartHint = notEnabled ? "在设置中启用 Agent" : authHint;
    return (
      <div className="flex h-full flex-col gap-3 p-4">
        <div className="rounded-lg border bg-card p-4">
          <div className="flex items-center gap-2">
            <Bot className="size-4" />
            <p className="text-sm font-medium">Codex Agent</p>
          </div>
          {isStarting ? (
            <p className="mt-2 text-xs text-muted-foreground">正在启动 Agent…</p>
          ) : isExited ? (
            <>
              <p className="mt-2 flex items-center gap-1.5 text-xs text-amber-700 dark:text-amber-300">
                <AlertTriangle className="size-3.5" />
                Agent 已退出
              </p>
              {status?.reason && <p className="mt-1 text-xs text-muted-foreground">{String(status.reason)}</p>}
              {status?.lastError && !status?.reason && (
                <p className="mt-1 text-xs text-muted-foreground">{String(status.lastError)}</p>
              )}
              {Array.isArray(status?.stderr_tail) && status.stderr_tail.length > 0 && (
                <pre className="mt-2 max-h-32 overflow-auto rounded bg-muted p-2 text-[11px]">
                  {(status.stderr_tail as string[]).slice(-20).join("\n")}
                </pre>
              )}
              {Array.isArray(status?.["stderrTail"]) && (status["stderrTail"] as string[]).length > 0 && !Array.isArray(status?.stderr_tail) && (
                <pre className="mt-2 max-h-32 overflow-auto rounded bg-muted p-2 text-[11px]">
                  {((status["stderrTail"] as string[]) ?? []).slice(-20).join("\n")}
                </pre>
              )}
            </>
          ) : notEnabled ? (
            <p className="mt-2 text-xs text-muted-foreground">在设置中启用 Agent 后即可启动本地 Codex 能力。</p>
          ) : authHint ? (
            <p className="mt-2 text-xs text-amber-700 dark:text-amber-300">{authHint}</p>
          ) : (
            <p className="mt-2 text-xs text-muted-foreground">
              未启用或未运行。点击下方按钮启动本地 Codex Agent（需已安装 codex 二进制）。
            </p>
          )}
          {preStartHint && !isExited && !isStarting && (
            <div className="mt-3 flex items-center gap-2">
              {onOpenSettings ? (
                <Button type="button" size="sm" variant="outline" className="gap-1.5" onClick={onOpenSettings}>
                  <Settings className="size-3.5" />
                  打开设置
                </Button>
              ) : (
                <span className="text-xs text-muted-foreground">{preStartHint}</span>
              )}
            </div>
          )}
          {statusError && <p className="mt-2 rounded bg-destructive/10 px-2 py-1 text-xs text-destructive">{statusError}</p>}
          <Button type="button" size="sm" className="mt-3" onClick={() => void handleStart()} disabled={isStarting}>
            {isStarting ? "启动中…" : isExited ? "重新启动 Agent" : "启动 Agent"}
          </Button>
          {phase === "stopped" && (
            <p className="mt-2 text-[11px] text-muted-foreground">若未检测到二进制，请在设置中配置 codex 路径或安装后重试。</p>
          )}
          {phase === "stopped" && preStartHint && onOpenSettings && (
            <p className="mt-1 text-[11px] text-muted-foreground">{preStartHint}</p>
          )}
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col gap-2 p-3">
      <div className="flex items-center gap-1">
        <Button
          type="button"
          variant={panelMode === "conversation" ? "secondary" : "ghost"}
          size="sm"
          className="h-7 gap-1.5 text-xs"
          onClick={() => setPanelMode("conversation")}
        >
          <MessageSquare className="size-3.5" />
          会话
        </Button>
        <Button
          type="button"
          variant={panelMode === "threadList" ? "secondary" : "ghost"}
          size="sm"
          className="h-7 gap-1.5 text-xs"
          onClick={() => setPanelMode("threadList")}
        >
          <ListIcon className="size-3.5" />
          列表
        </Button>
        <span className="ml-auto truncate text-[11px] text-muted-foreground">{view.threadId ? view.threadId.slice(0, 8) : "新会话"}</span>
      </div>

      {panelMode === "threadList" ? (
        <div className="min-h-0 flex-1 overflow-hidden rounded-md border">
          <ThreadList
            vaultRoot={vaultRoot}
            activeThreadId={view.threadId || null}
            isRunning={isRunning}
            onSelectThread={handleSelectThread}
            onNewThread={handleNewThread}
            onArchived={handleArchived}
            model={selectedModel}
            onModelChange={handleModelChange}
          />
        </div>
      ) : (
        <>
          <div ref={listRef} className="flex min-h-0 flex-1 flex-col gap-3 overflow-auto rounded-md border bg-card p-3">
            <MessageStream items={view.items} activeTurnId={view.activeTurnId} onInsertToNote={onInsertToNote} />
            {error && <p className="rounded bg-destructive/10 px-2 py-1 text-xs text-destructive">{error}</p>}
          </div>

          <div className="flex gap-2">
            <textarea
              value={input}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={handleKeyDown}
              placeholder={view.activeTurnId ? "输入追加消息，Enter 发送（steer），Esc 中断" : "输入消息，Enter 发送，Shift+Enter 换行"}
              className="max-h-28 min-h-[44px] flex-1 resize-none rounded-md border border-input bg-background px-3 py-2 text-sm placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              rows={2}
            />
            {view.activeTurnId ? (
              <Button
                type="button"
                variant="secondary"
                size="icon"
                className="size-9 shrink-0"
                onClick={() => void handleInterrupt()}
                aria-label="中断"
                title="中断（Esc）"
              >
                <Square className="size-3.5" />
              </Button>
            ) : (
              <Button type="button" size="icon" className="size-9 shrink-0" onClick={() => void handleSend()} disabled={!input.trim()} aria-label="发送">
                <Send className="size-4" />
              </Button>
            )}
          </div>
          {view.activeTurnId && <p className="text-[11px] text-muted-foreground">Agent 正在执行… 可输入追加消息或按 Esc 中断</p>}
        </>
      )}

      {queue.length > 0 && (
        <ApprovalDialog queue={queue} view={view} onRespond={handleApprovalRespond} allowlist={allowlist} onAllowlistChanged={bumpAllowlist} />
      )}
    </div>
  );
}
