/**
 * Agent 面板 —— M2 完整版 + M3 引导联动 + Codex CLI 风格 + 上下文注入 + 错误横幅 + ScrollArea
 * 审批弹窗 + MessageStream + ThreadList + vault 一致性闭环
 * M3：未启用/认证缺失/exited 摘要引导，onOpenSettings 联动
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Bot, Send, Square, AlertTriangle, List as ListIcon, MessageSquare, Settings, X, FileText, Pencil } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  getStatus,
  start,
  turnStart,
  turnSteer,
  turnInterrupt,
  threadStart,
  threadSetName,
  threadArchive,
  threadCompactStart,
  reviewStart,
  threadTurnsList,
  agentRespond,
  onAgentNotification,
  onAgentStatus,
  onParseError,
  onServerRequest,
  getSettings,
  getAuthStatus,
} from "@/lib/agent/client";
import type { AgentSettingsDto, AgentStatusDto, ParseErrorPayload } from "@/lib/agent/client";
import {
  createInitialView,
  reduceAgentEvent,
  reduceServerRequest,
  resolveServerRequest,
  hydrateTurnsToItems,
} from "@/lib/agent/codec";
import type { AgentThreadView, ApprovalRequestView, ItemView } from "@/lib/agent/codec";
import {
  addThread,
  listThreads,
  updateThreadModel,
  updateThreadTitle,
  updateThreadMode,
  updateThreadEffort,
  archiveThread,
} from "@/lib/agent/store";
import type { ServerNotification } from "@/lib/agent/types";
import { ApprovalDialog } from "@/components/agent/ApprovalDialog";
import { MessageStream } from "@/components/agent/MessageStream";
import { ThreadList } from "@/components/agent/ThreadList";
import type { ThreadResumeResult } from "@/components/agent/ThreadList";
import { createAllowlist, allowlistHas, allowlistAdd } from "@/lib/agent/allowlist";
import type { Allowlist } from "@/lib/agent/allowlist";
import { setAiModel } from "@/lib/ai-config";
import type { NoteDto } from "@/api/types";
import { enqueue as queueEnqueue, remove as queueRemove, onTurnCompleted } from "@/lib/agent/queue";
import type { QueueItem } from "@/lib/agent/queue";
import { MAX_QUEUE_SIZE } from "@/lib/agent/queue";
import {
  DEFAULT_AGENT_MODE,
  isAgentMode,
  modeToTurnOverrides,
} from "@/lib/agent/mode-presets";
import type { AgentMode } from "@/lib/agent/mode-presets";
import { parseSlashInput, filterCommands, applyPlanCommand } from "@/lib/agent/slash-commands";
import type { Command } from "@/lib/agent/slash-commands";
import type { SlashMenuHandle } from "@/components/agent/SlashMenu";
import type { ModelOption } from "@/lib/agent/models";
import { listAvailableModels } from "@/lib/agent/models";
import { ModeBar } from "@/components/agent/ModeBar";
import { ModelMenu } from "@/components/agent/ModelMenu";
import { SlashMenu } from "@/components/agent/SlashMenu";
import { QueuedChips } from "@/components/agent/QueuedChips";
import { TurnDiffBar } from "@/components/agent/TurnDiffBar";
import { DiffReviewDialog } from "@/components/agent/DiffReviewDialog";

const NOTE_CONTEXT_LIMIT = 8000;
const NOTE_CONTEXT_TRUNCATED_HINT = "（内容已截断）";
const PARSE_ERROR_DEDUP_MS = 3000;

function isTauriEnv(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

function buildNotePrefix(key: string, body: string): string {
  const needsTrunc = body.length > NOTE_CONTEXT_LIMIT;
  const snapshot = needsTrunc ? body.slice(0, NOTE_CONTEXT_LIMIT) : body;
  const truncatedMark = needsTrunc ? `\n${NOTE_CONTEXT_TRUNCATED_HINT}` : "";
  return `[当前打开的笔记: ${key}.md（vault 根目录相对路径，可用文件工具直接读写）]\n<note_content>\n${snapshot}${truncatedMark}\n</note_content>`;
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

function formatTokenUsage(tokenUsage: unknown): string | null {
  if (tokenUsage == null || typeof tokenUsage !== "object") return null;
  const u = tokenUsage as Record<string, unknown>;
  // 标准 ThreadTokenUsage：{ total: { totalTokens, inputTokens, outputTokens }, last: {...} }
  const total = u["total"] as Record<string, unknown> | undefined;
  if (total && typeof total["totalTokens"] === "number") {
    const t = total["totalTokens"] as number;
    const input = typeof total["inputTokens"] === "number" ? (total["inputTokens"] as number) : null;
    const output = typeof total["outputTokens"] === "number" ? (total["outputTokens"] as number) : null;
    if (input != null && output != null) return `tokens ${t} (in ${input} / out ${output})`;
    return `tokens ${t}`;
  }
  // 兼容扁平
  if (typeof u["totalTokens"] === "number") return `tokens ${u["totalTokens"] as number}`;
  return null;
}

/**
 * 2E：审批模式 → ThreadStartParams.sandbox（SandboxMode 字符串）。
 * ThreadStartParams 不支持 sandboxPolicy 对象（独有该字段），
 * 与 turnStart 的 sandboxPolicy 覆盖语义对应：
 * 只读=read-only ／ 自动=workspace-write ／ 全权=danger-full-access。
 */
function agentModeToSandbox(mode: AgentMode): "read-only" | "workspace-write" | "danger-full-access" {
  switch (mode) {
    case "readonly":
      return "read-only";
    case "auto":
      return "workspace-write";
    case "full":
      return "danger-full-access";
  }
}

/** 2E：限流快照 → 状态栏 title 文案（hover 可见，不刷消息流） */
function formatRateLimitsTitle(rateLimits: unknown): string | null {
  if (rateLimits == null || typeof rateLimits !== "object") return null;
  let body: string;
  try {
    body = JSON.stringify(rateLimits);
  } catch {
    return null;
  }
  const short = body.length > 600 ? `${body.slice(0, 600)}…` : body;
  return `限流状态（account/rateLimits/updated）：${short}`;
}

type Props = {
  onInsertToNote: (text: string) => void;
  vaultRoot: string | null;
  onVaultChanged: () => void;
  flushSave: () => Promise<void>;
  onOpenSettings?: () => void;
  noteKey?: string | null;
  getCurrentNote?: () => Promise<NoteDto | null>;
};

type PanelMode = "threadList" | "conversation";

export function AgentPanel({
  onInsertToNote,
  vaultRoot,
  onVaultChanged,
  flushSave,
  onOpenSettings,
  noteKey = null,
  getCurrentNote,
}: Props) {
  const isDesktop = isTauriEnv();

  const [status, setStatus] = useState<AgentStatusDto | null>(null);
  const [statusLoading, setStatusLoading] = useState(true);
  const [statusError, setStatusError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  const [view, setView] = useState<AgentThreadView>(() => createInitialView());
  const [input, setInput] = useState("");
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [errorDetails, setErrorDetails] = useState<string | null>(null);
  const [threadTitle, setThreadTitle] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState("");
  const [renameOpen, setRenameOpen] = useState(false);
  const [renameSaving, setRenameSaving] = useState(false);
  const [renameError, setRenameError] = useState<string | null>(null);
  const [queue, setQueue] = useState<ApprovalRequestView[]>([]);
  const [allowlist] = useState<Allowlist>(() => createAllowlist());
  const [allowlistVersion, setAllowlistVersion] = useState(0);
  const [panelMode, setPanelMode] = useState<PanelMode>("conversation");
  const [selectedModel, setSelectedModel] = useState<string | null>(null);
  const [agentSettings, setAgentSettings] = useState<AgentSettingsDto | null>(null);
  const [chatAuthed, setChatAuthed] = useState<boolean | null>(null);
  const [noteContextEnabled, setNoteContextEnabled] = useState(true);
  // 2E：审批模式（按 thread 持久化，运行中切换只影响下一回合）
  const [agentMode, setAgentMode] = useState<AgentMode>(DEFAULT_AGENT_MODE);
  // 2E：推理强度（store thread record 持久化，null = 跟随模型默认）
  const [effort, setEffortState] = useState<string | null>(null);
  // 2E：消息排队（session-only，刷新丢失，不持久化）
  const [sendQueue, setSendQueue] = useState<QueueItem[]>([]);
  // 2E：斜杠命令菜单开关 / 模型菜单开关
  const [slashOpen, setSlashOpen] = useState(false);
  const [slashQuery, setSlashQuery] = useState("");
  const [modelMenuOpen, setModelMenuOpen] = useState(false);
  const [panelModels, setPanelModels] = useState<ModelOption[]>([]);
  const [panelModelsLoading, setPanelModelsLoading] = useState(false);
  const [planHint, setPlanHint] = useState<string | null>(null);
  // 2E：diff 审查（turnId → 弹层态；dismissedIds 记录已接受/已处理的 turn）
  const [dismissedDiffIds, setDismissedDiffIds] = useState<Set<string>>(new Set());
  const [reviewTurnId, setReviewTurnId] = useState<string | null>(null);

  const slashMenuRef = useRef<SlashMenuHandle | null>(null);

  const fileChangeInTurnRef = useRef(false);
  const allowlistRef = useRef(allowlist);
  useEffect(() => {
    allowlistRef.current = allowlist;
  }, [allowlist]);

  const listRef = useRef<HTMLDivElement>(null);
  const parseErrorDedupRef = useRef<{ key: string; at: number } | null>(null);
  const viewRef = useRef(view);
  useEffect(() => {
    viewRef.current = view;
  }, [view]);
  // 2E：单次订阅的通知回调经 ref 读取最新队列/提交函数，避免闭包过期
  const queueRef = useRef<QueueItem[]>([]);
  const submitTextRef = useRef<(text: string, steerIfActive: boolean) => Promise<void>>(() =>
    Promise.resolve(),
  );
  const sendQueuedTurnRef = useRef<() => Promise<void>>(() => Promise.resolve());
  // 长时间监听的事件回调内取 vaultRoot：避免把 vaultRoot 放进 effect 依赖导致频繁重订阅
  const vaultRootRef = useRef(vaultRoot);
  useEffect(() => {
    vaultRootRef.current = vaultRoot;
  }, [vaultRoot]);

  // 切笔记后 chip 跟随更新并重置为附加状态
  useEffect(() => {
    if (noteKey) setNoteContextEnabled(true);
  }, [noteKey]);

  useEffect(() => {
    // 切会话 / 新建会话：从 store record 回读 model/mode/effort（缺省默认档/跟随模型）
    if (!vaultRoot) return;
    try {
      const rec = view.threadId ? listThreads(vaultRoot).find((r) => r.threadId === view.threadId) : undefined;
      if (rec?.model) setSelectedModel(rec.model);
      const storedMode = rec?.mode;
      setAgentMode(storedMode && isAgentMode(storedMode) ? storedMode : DEFAULT_AGENT_MODE);
      setEffortState(typeof rec?.effort === "string" ? (rec.effort as string) : null);
    } catch {
      // ignore
    }
  }, [vaultRoot, view.threadId]);

  // 2E：面板内模型列表（ModeBar modelSlot 用；失败不整页报错）
  useEffect(() => {
    if (!isDesktop) return;
    const authMode = String(agentSettings?.authMode ?? "gateway");
    // 非 gateway 模式需 codex 运行后才能 model/list；gateway 走服务端 relay
    if (authMode !== "gateway" && status?.phase !== "running") return;
    let cancelled = false;
    setPanelModelsLoading(true);
    listAvailableModels(authMode)
      .then((list) => {
        if (!cancelled) setPanelModels(list);
      })
      .catch(() => {
        if (!cancelled) setPanelModels([]);
      })
      .finally(() => {
        if (!cancelled) setPanelModelsLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [isDesktop, agentSettings?.authMode, status?.phase]);

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
    let unlistenParse: (() => void) | null = null;
    let cancelled = false;

    void onAgentStatus((s) => {
      if (cancelled) return;
      setStatus(s);
      if (s.phase === "exited" || s.phase === "stopped") {
        setView((prev) => ({ ...prev, status: "idle", activeTurnId: null }));
        setQueue([]);
        // B.3：在会话视图时设置错误横幅（避免静默 exited）
        if (s.phase === "exited" && viewRef.current.threadId) {
          const reason = typeof s.reason === "string" && s.reason.trim() ? s.reason.trim() : null;
          const lastErr = typeof s.lastError === "string" && s.lastError.trim() ? s.lastError.trim() : null;
          const code = typeof s.code === "number" ? `（退出码 ${s.code}）` : "";
          const msg = reason ?? lastErr ?? `Agent 已退出${code}`;
          setError(msg);
          const tails: string[] = [];
          if (Array.isArray(s.stderr_tail) && s.stderr_tail.length > 0) tails.push(...(s.stderr_tail as string[]));
          if (Array.isArray((s as Record<string, unknown>)["stderrTail"])) {
            const alt = (s as Record<string, unknown>)["stderrTail"] as string[];
            if (alt.length > 0 && tails.length === 0) tails.push(...alt);
          }
          if (tails.length > 0) setErrorDetails(tails.slice(-40).join("\n"));
          else setErrorDetails(null);
        }
      }
    }).then((fn) => {
      unlistenStatus = fn;
    });

    void onAgentNotification((n: ServerNotification) => {
      if (cancelled) return;
      const method = (n as unknown as { method: string }).method;
      setView((prev) => reduceAgentEvent(prev, n));
      const params = (n as unknown as { params: unknown }).params as Record<string, unknown> | null | undefined;
      // 2C：thread/name/updated → 同步头部显示名 + 本地 store 标题（含 codex 侧自动标题）
      if (method === "thread/name/updated") {
        const p = (params ?? {}) as { threadId?: unknown; threadName?: unknown };
        const tid = typeof p["threadId"] === "string" ? (p["threadId"] as string) : null;
        const nm = typeof p["threadName"] === "string" && p["threadName"].trim() ? p["threadName"].trim() : null;
        if (tid && nm) {
          if (viewRef.current.threadId === tid) setThreadTitle(nm);
          const vr = vaultRootRef.current;
          if (vr) {
            try {
              updateThreadTitle(vr, tid, nm);
            } catch {
              // ignore
            }
          }
        }
      }
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
        // 2E：回合完成（codec 置 idle 的同一点）→ 弹出队首自动走发送管线
        void sendQueuedTurnRef.current();
      }
      requestAnimationFrame(() => {
        if (listRef.current) listRef.current.scrollTop = listRef.current.scrollHeight;
      });
    }).then((fn) => {
      unlistenNotif = fn;
    });

    void onParseError((payload: ParseErrorPayload) => {
      if (cancelled) return;
      // 去抖/去重：同 payload 短时间内不重复刷屏
      const key = `${payload.error}::${payload.line.slice(0, 120)}`;
      const now = Date.now();
      const prev = parseErrorDedupRef.current;
      if (prev && prev.key === key && now - prev.at < PARSE_ERROR_DEDUP_MS) return;
      parseErrorDedupRef.current = { key, at: now };
      // 仅在有会话上下文时打到会话内横幅，避免空会话刷屏
      setError(`解析错误：${payload.error}`);
      setErrorDetails(payload.line.length > 800 ? `${payload.line.slice(0, 800)}…` : payload.line);
      // 同时注入一条 error item 便于会话内留痕
      setView((prev) => {
        if (!prev.threadId) return prev;
        const next = reduceAgentEvent(prev, { method: "error", params: { error: { message: payload.error, additionalDetails: payload.line.slice(0, 400), codexErrorInfo: null, misalignment: null }, willRetry: false, threadId: prev.threadId, turnId: prev.activeTurnId ?? "" } } as unknown as ServerNotification);
        // reduceAgentEvent 对 error 通知的处理由 codec 完成
        return next;
      });
    }).then((fn) => {
      unlistenParse = fn;
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
        void sendAuto().catch((e: unknown) => {
          const msg = e instanceof Error ? e.message : String(e);
          setError(`自动批准失败：${msg}`);
        });
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
      unlistenParse?.();
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
      void agentRespond(id, result, errorVal).catch((e: unknown) => {
        const msg = e instanceof Error ? e.message : String(e);
        setError(`审批响应失败：${msg}`);
      });
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

  /** resume 成功时用回填 items 替换占位；顶部插「摘要回放」system 分隔 */
  const handleSelectThread = useCallback(
    (threadId: string, resumed: ThreadResumeResult | null) => {
      const now = Date.now();
      const divider: ItemView = {
        kind: "system",
        id: `history-divider-${now}`,
        text: "以下为历史消息（摘要回放，只读）",
        tone: "info",
      };
      const backfilled = resumed?.items ?? [];
      const title = resumed?.title ?? null;
      if (resumed) {
        const items = backfilled.length > 0 ? [divider, ...backfilled] : [];
        setView((prev) => ({
          ...prev,
          threadId,
          // 同会话重入时保留当前流内消息，避免回退覆盖新回合；
          // 跨会话切换时同时丢弃旧 turnDiffs（2E，转 diff 快照与会话绑定）
          items: prev.threadId === threadId ? prev.items : items,
          turnDiffs: prev.threadId === threadId ? prev.turnDiffs : [],
          activeTurnId: null,
          status: "idle",
        }));
        setThreadTitle(title);
        if (vaultRoot && title) {
          try {
            updateThreadTitle(vaultRoot, threadId, title);
          } catch {
            // ignore
          }
        }
      } else {
        // resume 失败仅本地切 threadId：清空消息流与 turnDiffs（无历史可展示）
        setView((prev) => ({
          ...prev,
          threadId,
          items: prev.threadId === threadId ? prev.items : [],
          turnDiffs: prev.threadId === threadId ? prev.turnDiffs : [],
          activeTurnId: null,
          status: "idle",
        }));
        // 保留本地已存标题，若无则回退 id 短码（由头部渲染决定）
        if (vaultRoot) {
          try {
            const rec = listThreads(vaultRoot).find((r) => r.threadId === threadId);
            setThreadTitle(rec?.title ?? null);
          } catch {
            setThreadTitle(null);
          }
        } else {
          setThreadTitle(null);
        }
      }
      setPanelMode("conversation");
      setError(null);
      setErrorDetails(null);
      // 2E：切换会话清空本地排队（session-only）与 diff 审查态；mode/effort 由回读 effect 同步
      setSendQueue([]);
      queueRef.current = [];
      setDismissedDiffIds(new Set());
      setReviewTurnId(null);
      setSlashOpen(false);
      setSlashQuery("");
      setModelMenuOpen(false);
      setPlanHint(null);
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
    setView((prev) => ({ ...prev, threadId: "", items: [], turnDiffs: [], activeTurnId: null, status: "idle" }));
    setThreadTitle(null);
    setRenameOpen(false);
    setRenameError(null);
    setError(null);
    setErrorDetails(null);
    setPanelMode("conversation");
    // 2E:新会话清空排队与审查态；mode/effort 回读 effect 会置回默认值
    setSendQueue([]);
    queueRef.current = [];
    setDismissedDiffIds(new Set());
    setReviewTurnId(null);
    setSlashOpen(false);
    setSlashQuery("");
    setModelMenuOpen(false);
    setPlanHint(null);
  }, []);

  const handleArchived = useCallback((threadId: string) => {
    if (viewRef.current.threadId === threadId) {
      setView((prev) => ({ ...prev, threadId: "", items: [], turnDiffs: [], activeTurnId: null, status: "idle" }));
      setThreadTitle(null);
      setRenameOpen(false);
      setRenameError(null);
      setSendQueue([]);
      queueRef.current = [];
      setDismissedDiffIds(new Set());
      setReviewTurnId(null);
    }
  }, []);

  /** 删除的是当前会话时切回列表（随后用户可新建），非当前则无需改动视图 */
  const handleThreadDeleted = useCallback((threadId: string) => {
    if (viewRef.current.threadId === threadId) {
      setView((prev) => ({ ...prev, threadId: "", items: [], turnDiffs: [], activeTurnId: null, status: "idle" }));
      setThreadTitle(null);
      setRenameOpen(false);
      setRenameError(null);
      setPanelMode("threadList");
      setSendQueue([]);
      queueRef.current = [];
      setDismissedDiffIds(new Set());
      setReviewTurnId(null);
      setSlashOpen(false);
      setSlashQuery("");
      setModelMenuOpen(false);
      setPlanHint(null);
    }
  }, []);

  /** 头部重命名：弹窗确认 → thread/name/set → 本地 store 同步 */
  const handleOpenRename = useCallback(() => {
    const cur = viewRef.current.threadId;
    if (!cur) return;
    const localTitle =
      vaultRoot != null
        ? (() => {
            try {
              return listThreads(vaultRoot).find((r) => r.threadId === cur)?.title ?? "";
            } catch {
              return "";
            }
          })()
        : "";
    const init = threadTitle ?? localTitle;
    setRenameDraft(init || "");
    setRenameError(null);
    setRenameOpen(true);
  }, [vaultRoot, threadTitle]);

  const handleSaveRename = useCallback(async () => {
    const cur = viewRef.current.threadId;
    const name = renameDraft.trim();
    if (!cur || !name) {
      setRenameError("名称不能为空");
      return;
    }
    setRenameSaving(true);
    setRenameError(null);
    try {
      await threadSetName({ threadId: cur, name });
      setThreadTitle(name);
      if (vaultRoot) {
        try {
          updateThreadTitle(vaultRoot, cur, name);
        } catch {
          // ignore
        }
      }
      setRenameOpen(false);
    } catch (e: unknown) {
      setRenameError(e instanceof Error ? e.message : String(e));
    } finally {
      setRenameSaving(false);
    }
  }, [renameDraft, vaultRoot]);

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
      if (next && String(agentSettings?.authMode ?? "gateway") === "gateway") {
        try {
          setAiModel(next);
        } catch {
          // ignore storage errors
        }
      }
    },
    [vaultRoot, agentSettings?.authMode],
  );

  // —— 2E 审批模式 / 推理强度 ——

  /** ModeBar 切换：setState + store.updateThreadMode（运行中切换只影响下一回合） */
  const handleModeChange = useCallback(
    (next: string) => {
      if (!isAgentMode(next)) return;
      setAgentMode(next);
      const tid = viewRef.current.threadId;
      if (vaultRoot && tid) {
        try {
          updateThreadMode(vaultRoot, tid, next);
        } catch {
          // ignore
        }
      }
    },
    [vaultRoot],
  );

  /** ModelMenu 模型选择：复用既有关模型逻辑（updateThreadModel + gateway 同步） */
  const handlePanelModelSelect = useCallback(
    (id: string) => {
      handleModelChange(id);
      setModelMenuOpen(false);
    },
    [handleModelChange],
  );

  /** ModelMenu 强度选择：store.updateThreadEffort + turnStart 附带 effort */
  const handleEffortSelect = useCallback(
    (next: string | null) => {
      setEffortState(next);
      const tid = viewRef.current.threadId;
      if (vaultRoot && tid) {
        try {
          updateThreadEffort(vaultRoot, tid, next);
        } catch {
          // ignore
        }
      }
    },
    [vaultRoot],
  );

  const buildTurnText = useCallback(
    async (rawText: string): Promise<string> => {
      if (!noteContextEnabled || !noteKey || !getCurrentNote) return rawText;
      try {
        const note = await getCurrentNote();
        if (!note || typeof note.body !== "string") return rawText;
        const prefix = buildNotePrefix(note.key ?? noteKey, note.body);
        return `${prefix}\n\n${rawText}`;
      } catch {
        return rawText;
      }
    },
    [noteContextEnabled, noteKey, getCurrentNote],
  );

  // 发送中 flag 的 ref 镜像：通知回调里取最新，避免闭包过期
  const sendingRef = useRef(sending);
  useEffect(() => {
    sendingRef.current = sending;
  }, [sending]);

  /**
   * 2E：单条消息的发送管线（Enter 空闲 / ⌘+Enter / 队列自动弹出共用）。
   * 仅文本参数；笔记上下文在发送时经 buildTurnText 实时注入（队列元素只存原文）。
   * steerIfActive=true 时若正处于活跃回合则走 turn/steer（⌘+Enter 立即插话），
   * 否则视有无活跃回合：新建会话走 thread/start（附带 mode/effort 覆盖），
   * 已有会话走 turn/start（附带 approvalPolicy/sandboxPolicy 覆盖）。
   */
  const submitText = useCallback(
    async (rawText: string, steerIfActive: boolean) => {
      const text = rawText.trim();
      if (!text || sendingRef.current) return;
      // 先置位再 await，杜绝快速连按/队列并发导致的重复发送
      sendingRef.current = true;
      setSending(true);
      try {
        const hint = getAuthMissingHint(agentSettings, chatAuthed);
        if (hint) {
          setError(hint);
          return;
        }
        setError(null);
        setErrorDetails(null);
        setPlanHint(null);
        try {
          await flushSave();
        } catch (e: unknown) {
          const msg = e instanceof Error ? e.message : String(e);
          console.warn("[agent] flushSave 失败：", msg);
        }

        const finalText = await buildTurnText(text);

        setView((prev) => {
          const exists = prev.items.some((it) => it.kind === "userMessage" && it.text === text);
          if (exists) return prev;
          return {
            ...prev,
            items: [...prev.items, { kind: "userMessage" as const, id: `local-${Date.now()}`, text }],
          };
        });
        setInput("");

        const cur = viewRef.current;
        let threadId = cur.threadId;
        const modelForStart = selectedModel ?? null;
        const effForStart = effort && effort.trim() ? effort : null;
        // mode → per-turn 覆盖（approvalPolicy + sandboxPolicy）；无 vault 根时不附加
        const overrides = vaultRoot ? modeToTurnOverrides(agentMode, vaultRoot) : null;
        if (steerIfActive && cur.activeTurnId && threadId) {
          // 运行中 ⌘+Enter：立即插话（TurnSteerParams 无覆盖字段，维持现状）
          await turnSteer({
            threadId,
            expectedTurnId: cur.activeTurnId,
            input: [{ type: "text", text: finalText, text_elements: [] }],
          });
          return;
        }
        if (!threadId) {
          const res = await threadStart({
            ...(modelForStart ? { model: modelForStart } : {}),
            ...(effForStart ? { effort: effForStart } : {}),
            // ThreadStartParams 用 sandbox: SandboxMode 字符串而非 sandboxPolicy 对象
            ...(overrides ? { approvalPolicy: overrides.approvalPolicy, sandbox: agentModeToSandbox(agentMode) } : {}),
          });
          threadId = res.thread.id;
          setView((prev) => ({ ...prev, threadId }));
          // 新会话头部直接落标题（body 前 32 字），重命名后再以最新为准
          setThreadTitle(text.slice(0, 32) || "新会话");
          if (vaultRoot) {
            try {
              const title = text.slice(0, 32) || "新会话";
              addThread(vaultRoot, {
                threadId,
                title,
                createdAt: Math.floor(Date.now() / 1000),
                model: (res.model as string | null) ?? modelForStart,
                mode: agentMode,
                effort: effForStart,
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
        await turnStart({
          threadId,
          input: [{ type: "text", text: finalText, text_elements: [] }],
          ...(modelForStart ? { model: modelForStart } : {}),
          ...(effForStart ? { effort: effForStart } : {}),
          ...(overrides ? { approvalPolicy: overrides.approvalPolicy, sandboxPolicy: overrides.sandboxPolicy } : {}),
        });
      } catch (e: unknown) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        sendingRef.current = false;
        setSending(false);
        requestAnimationFrame(() => {
          if (listRef.current) listRef.current.scrollTop = listRef.current.scrollHeight;
        });
      }
    },
    [flushSave, vaultRoot, agentMode, effort, selectedModel, agentSettings, chatAuthed, buildTurnText],
  );

  // 队列 / 提交函数的最新引用同步到通知回调可读的 ref（保持订阅 effect 依赖稳定）
  useEffect(() => {
    queueRef.current = sendQueue;
  }, [sendQueue]);
  useEffect(() => {
    submitTextRef.current = submitText;
  }, [submitText]);

  // —— 2E 消息排队（session-only） ——

  /** Enter 提交（空闲=发送；运行中=入队并清空输入框） */
  const handleSend = useCallback(() => {
    const text = input.trim();
    if (!text || sending) return;
    if (view.activeTurnId) {
      const { queue: next, item } = queueEnqueue(sendQueue, text);
      if (!item) {
        setPlanHint(`队列已满（最多 ${MAX_QUEUE_SIZE} 条），先发送或删除队列中的消息`);
        return;
      }
      setSendQueue(next);
      setInput("");
      setError(null);
      setErrorDetails(null);
      setPlanHint(null);
      return;
    }
    void submitText(text, false);
  }, [input, sending, view.activeTurnId, sendQueue, submitText]);

  /** 运行中 ⌘/Ctrl+Enter：立即 steer（无队列语义） */
  const handleSendSteer = useCallback(() => {
    const text = input.trim();
    if (!text || sending) return;
    if (view.activeTurnId) void submitText(text, true);
    else void submitText(text, false);
  }, [input, sending, view.activeTurnId, submitText]);

  /** 出队并立即发送（无笔记上下文注入外的副作用）——turn/completed 后调用 */
  const sendQueuedTurn = useCallback(async () => {
    const { next, toSend } = onTurnCompleted(queueRef.current);
    if (!toSend) return;
    queueRef.current = next;
    setSendQueue(next);
    await submitText(toSend.text, false);
  }, [submitText]);
  useEffect(() => {
    sendQueuedTurnRef.current = sendQueuedTurn;
  }, [sendQueuedTurn]);

  /** 删除单条排队消息 */
  const handleQueueRemove = useCallback((id: string) => {
    setSendQueue((prev) => queueRemove(prev, id));
  }, []);

  // —— 2E diff 审查 ——

  /** 回合完成且有聚合 diff 时展示 TurnDiffBar；已处理（接受/撤销）的 turn 不再展示 */
  const activeDiffBarTurn =
    !view.activeTurnId && view.turnDiffs.length > 0
      ? view.turnDiffs[view.turnDiffs.length - 1]
      : null;
  const showDiffBar = activeDiffBarTurn ? !dismissedDiffIds.has(activeDiffBarTurn.turnId) : false;

  const dismissDiffTurn = useCallback((turnId: string) => {
    setDismissedDiffIds((prev) => {
      if (prev.has(turnId)) return prev;
      const next = new Set(prev);
      next.add(turnId);
      return next;
    });
  }, []);
  const openReview = useCallback((turnId: string) => setReviewTurnId(turnId), []);
  const closeReview = useCallback(() => setReviewTurnId(null), []);
  // 拒绝即进入 DiffReviewDialog 由用户勾选文件撤销（组件接口无独立 reject 预选态）
  const onRejectDiff = useCallback((turnId: string) => setReviewTurnId(turnId), []);
  // 接受：文件本就已落盘，仅清除审查态
  const onAcceptDiff = useCallback((turnId: string) => dismissDiffTurn(turnId), [dismissDiffTurn]);
  // 文件撤销成功：刷新 vault 文件树 + 清除审查态（该 turn 视为已处理）
  const onFilesReverted = useCallback(
    (paths: string[]) => {
      void paths;
      if (reviewTurnId) dismissDiffTurn(reviewTurnId);
      onVaultChanged();
    },
    [reviewTurnId, dismissDiffTurn, onVaultChanged],
  );
  // 对话历史回滚成功：经 turns/list 回填刷新消息流（服务端已删回合，本地视图须重拉）
  const onThreadReverted = useCallback(async (threadId: string) => {
    if (reviewTurnId) dismissDiffTurn(reviewTurnId);
    setThreadTitle(null);
    onVaultChanged();
    if (threadId !== viewRef.current.threadId) return;
    setError(null);
    setErrorDetails(null);
    try {
      const tl = await threadTurnsList({
        threadId,
        itemsView: "full",
        sortDirection: "asc",
        limit: 50,
      });
      const now = Date.now();
      const divider: ItemView = {
        kind: "system",
        id: `history-divider-${now}`,
        text: "以下为历史消息（摘要回放，只读）",
        tone: "info",
      };
      const items = hydrateTurnsToItems(tl.data);
      setView((prev) =>
        prev.threadId === threadId
          ? { ...prev, items: items.length > 0 ? [divider, ...items] : [], activeTurnId: null, status: "idle" }
          : prev,
      );
    } catch (e: unknown) {
      setError(`回滚后刷新消息流失败：${e instanceof Error ? e.message : String(e)}`);
    }
  }, [reviewTurnId, dismissDiffTurn, onVaultChanged]);

  // —— 2E 斜杠命令 ——

  const handleInputChange = useCallback(
    (text: string) => {
      setInput(text);
      setPlanHint(null);
      if (!text.startsWith("/") || text.includes("\n")) {
        setSlashOpen(false);
        setSlashQuery("");
        return;
      }
      const parsed = parseSlashInput(text);
      const hasThread = Boolean(viewRef.current.threadId);
      const commands = filterCommands(parsed?.cmd ?? "", { hasThread });
      if (commands.length === 0) {
        setSlashOpen(false);
        setSlashQuery("");
      } else {
        setSlashOpen(true);
        setSlashQuery(parsed?.cmd ?? "");
      }
    },
    [],
  );

  const handleSlashSelect = useCallback(
    (cmd: Command) => {
      const parsed = parseSlashInput(input);
      const arg = parsed?.arg ?? "";
      const threadId = viewRef.current.threadId;
      const run = async () => {
        switch (cmd.runKind) {
          case "rpc": {
            if (!threadId) {
              setError("该命令需要一个会话，请先发送消息或从列表恢复旧会话");
              return;
            }
            try {
              if (cmd.name === "compact") {
                // 压缩上下文后 codex 侧发 thread/compacted → codec 落 system item，无需本地补水
                await threadCompactStart({ threadId });
              } else if (cmd.name === "review") {
                await reviewStart({
                  threadId,
                  target: arg
                    ? { type: "custom", instructions: arg }
                    : { type: "uncommittedChanges" },
                });
              } else if (cmd.name === "rename") {
                if (!arg) {
                  setError(`用法：/rename <name>（缺少新名称）`);
                  return;
                }
                await threadSetName({ threadId, name: arg });
                setThreadTitle(arg);
                if (vaultRoot) {
                  try {
                    updateThreadTitle(vaultRoot, threadId, arg);
                  } catch {
                    // ignore
                  }
                }
              } else if (cmd.name === "archive") {
                await threadArchive({ threadId });
                if (vaultRoot) {
                  try {
                    archiveThread(vaultRoot, threadId);
                  } catch {
                    // ignore
                  }
                }
                handleArchived(threadId);
              }
            } catch (e: unknown) {
              setError(e instanceof Error ? e.message : String(e));
            }
            return;
          }
          case "ui": {
            if (cmd.name === "new") handleNewThread();
            else if (cmd.name === "model") setModelMenuOpen(true);
            return;
          }
          case "prefix": {
            if (cmd.name === "plan") {
              const { value, hint } = applyPlanCommand(input);
              setInput(value);
              setPlanHint(hint);
            }
            return;
          }
        }
      };
      setSlashOpen(false);
      setSlashQuery("");
      if (cmd.runKind !== "prefix") setInput("");
      void run();
    },
    [input, vaultRoot, handleArchived, handleNewThread],
  );

  /** textarea onKeyDown：斜杠菜单键盘事件最优先，其次 Enter/steer 语义，Esc 中断兜底 */
  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (slashOpen && slashMenuRef.current?.handleKeyDown(e)) {
      // SlashMenu 已消费 ↑↓/Enter/Esc，跳过后继发送/中断逻辑
      return;
    }
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      if (e.metaKey || e.ctrlKey) handleSendSteer();
      else handleSend();
      return;
    }
    if (e.key === "Escape" && view.activeTurnId) {
      e.preventDefault();
      void handleInterrupt();
    }
  };

  const handleInterrupt = useCallback(async () => {
    const cur = viewRef.current;
    if (!cur.threadId || !cur.activeTurnId) return;
    try {
      await turnInterrupt({ threadId: cur.threadId, turnId: cur.activeTurnId });
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

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

  const tokenText = formatTokenUsage(view.tokenUsage);
  const turnStateLabel = view.activeTurnId ? (view.status === "running" ? "运行中" : "思考中") : "空闲";
  const showNoteChip = Boolean(noteKey && noteContextEnabled);
  // 2E：限流快照 → 状态栏 token 文本 title（hover 可见，不刷消息流）
  const rateLimitsTitle = formatRateLimitsTitle(view.rateLimits);
  const tokenTitle = rateLimitsTitle ? (tokenText ? `${tokenText}\n${rateLimitsTitle}` : rateLimitsTitle) : (tokenText ?? undefined);
  // 2E：斜杠菜单可选项（无命令匹配时不渲染）
  const slashParsed = slashOpen ? parseSlashInput(input) : null;
  const slashCommands = slashOpen ? filterCommands(slashParsed?.cmd ?? "", { hasThread: Boolean(view.threadId) }) : [];
  // 2E：输入框占位文案（运行中 Enter 语义变更为排队，Cmd/Ctrl+Enter 保留立即 steer）
  const inputPlaceholder = view.activeTurnId
    ? "排队中… Enter 排队，⌘/Ctrl+⏎ 立即插话，Shift+Enter 换行，Esc 中断"
    : "输入消息，Enter 发送，Shift+Enter 换行（/ 查看命令）";
  const modelTriggerLabel = panelModelsLoading
    ? "模型加载中…"
    : (selectedModel ?? "未选模型");
  const reviewDiff = reviewTurnId ? (view.turnDiffs.find((t) => t.turnId === reviewTurnId)?.diff ?? null) : null;

  return (
    <div className="flex h-full flex-col gap-0">
      <div className="flex items-center gap-1 border-b border-border/50 px-2 py-1.5">
        <Button
          type="button"
          variant={panelMode === "conversation" ? "secondary" : "ghost"}
          size="sm"
          className="h-7 gap-1.5 rounded-md text-xs"
          onClick={() => setPanelMode("conversation")}
        >
          <MessageSquare className="size-3.5" />
          会话
        </Button>
        <Button
          type="button"
          variant={panelMode === "threadList" ? "secondary" : "ghost"}
          size="sm"
          className="h-7 gap-1.5 rounded-md text-xs"
          onClick={() => setPanelMode("threadList")}
        >
          <ListIcon className="size-3.5" />
          列表
        </Button>
        <span className="ml-auto flex min-w-0 items-center gap-1 font-mono text-[11px] text-muted-foreground">
          <span className="truncate" title={view.threadId || "新会话"}>
            {view.threadId ? (threadTitle || view.threadId.slice(0, 8)) : "新会话"}
          </span>
          {view.threadId && (
            <button
              type="button"
              onClick={handleOpenRename}
              className="shrink-0 rounded p-0.5 hover:bg-accent hover:text-foreground"
              aria-label="重命名会话"
              title="重命名会话"
            >
              <Pencil className="size-3" />
            </button>
          )}
        </span>
      </div>

      {renameOpen && view.threadId && (
        <div className="mx-2 mt-2 rounded-md border bg-card p-2.5">
          <p className="text-xs font-medium">重命名会话</p>
          <div className="mt-1.5 flex items-center gap-2">
            <input
              autoFocus
              value={renameDraft}
              onChange={(e) => setRenameDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  void handleSaveRename();
                }
                if (e.key === "Escape") {
                  e.preventDefault();
                  setRenameOpen(false);
                  setRenameError(null);
                }
              }}
              placeholder="输入会话名称"
              maxLength={80}
              className="h-8 min-w-0 flex-1 rounded-md border border-input bg-background px-2 text-xs placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
            />
            <Button type="button" size="sm" className="h-8" onClick={() => void handleSaveRename()} disabled={renameSaving || !renameDraft.trim()}>
              {renameSaving ? "保存中…" : "保存"}
            </Button>
            <Button
              type="button"
              size="sm"
              variant="ghost"
              className="h-8"
              onClick={() => {
                setRenameOpen(false);
                setRenameError(null);
              }}
            >
              取消
            </Button>
          </div>
          {renameError && <p className="mt-1.5 text-xs text-destructive">{renameError}</p>}
        </div>
      )}

      {panelMode === "threadList" ? (
        <div className="min-h-0 flex-1 overflow-hidden">
          <ThreadList
            vaultRoot={vaultRoot}
            activeThreadId={view.threadId || null}
            isRunning={isRunning}
            authMode={String(agentSettings?.authMode ?? "gateway")}
            onSelectThread={handleSelectThread}
            onNewThread={handleNewThread}
            onArchived={handleArchived}
            onDeleted={handleThreadDeleted}
            model={selectedModel}
            onModelChange={handleModelChange}
            onOpenSettings={onOpenSettings}
          />
        </div>
      ) : (
        <>
          {error && (
            <div className="mx-2 mt-2 overflow-hidden rounded-md border border-red-500/25 bg-red-500/10">
              <div className="flex gap-2 px-2.5 py-2">
                <AlertTriangle className="mt-0.5 size-3.5 shrink-0 text-red-600 dark:text-red-400" />
                <div className="min-w-0 flex-1">
                  <p className="whitespace-pre-wrap break-words text-xs font-medium text-red-700 dark:text-red-300">{error}</p>
                  {errorDetails && (
                    <details className="mt-1.5">
                      <summary className="cursor-pointer text-[11px] text-red-700/80 dark:text-red-300/80">查看详情</summary>
                      <pre className="mt-1 max-h-40 overflow-auto whitespace-pre-wrap break-words rounded bg-black/5 px-2 py-1.5 font-mono text-[11px] dark:bg-white/5">
                        {errorDetails.slice(0, 4000)}
                      </pre>
                    </details>
                  )}
                </div>
                <button
                  type="button"
                  onClick={() => {
                    setError(null);
                    setErrorDetails(null);
                  }}
                  className="shrink-0 rounded p-1 text-red-700/70 hover:bg-red-500/10 hover:text-red-700 dark:text-red-300/70"
                  aria-label="关闭"
                >
                  <X className="size-3.5" />
                </button>
              </div>
            </div>
          )}

          <ScrollArea className="min-h-0 flex-1" viewportRef={listRef}>
            <div className="px-3 py-2">
              <MessageStream items={view.items} activeTurnId={view.activeTurnId} onInsertToNote={onInsertToNote} />
            </div>
          </ScrollArea>

          {showNoteChip && (
            <div className="mx-2 mb-1 flex">
              <span className="inline-flex max-w-full items-center gap-1.5 rounded-full border bg-muted/60 px-2.5 py-1 text-[11px] text-muted-foreground">
                <FileText className="size-3 shrink-0" />
                <span className="max-w-[14rem] truncate font-mono">{noteKey}.md</span>
                <button
                  type="button"
                  onClick={() => setNoteContextEnabled(false)}
                  className="ml-0.5 rounded-full p-0.5 hover:bg-accent hover:text-foreground"
                  aria-label="移除上下文"
                  title="本次会话不再附加此笔记"
                >
                  <X className="size-3" />
                </button>
              </span>
            </div>
          )}

          {/* 2E：排队 chip + turn 审查条（输入框上方） */}
          <QueuedChips items={sendQueue} onRemove={handleQueueRemove} />
          {showDiffBar && activeDiffBarTurn && (
            <TurnDiffBar
              turnDiffs={view.turnDiffs}
              turnId={activeDiffBarTurn.turnId}
              dismissed={false}
              onReview={openReview}
              onAccept={onAcceptDiff}
              onReject={onRejectDiff}
            />
          )}
          {planHint && (
            <p className="mx-2 mb-1 rounded-md border border-amber-500/25 bg-amber-500/10 px-2.5 py-1.5 text-[11px] leading-snug text-amber-700 dark:text-amber-300">
              {planHint}
            </p>
          )}

          <div className="border-t border-border/40 px-2 pb-2 pt-2">
            {/* 2E：审批模式三档（运行中切换只影响下一回合；模型槽位挂 ModelMenu） */}
            <div className="mb-1.5">
              <ModeBar
                mode={agentMode}
                onModeChange={handleModeChange}
                vaultRoot={vaultRoot ?? ""}
                disabled={Boolean(view.activeTurnId)}
                modelSlot={
                  <div className="relative">
                    <button
                      type="button"
                      onClick={() => setModelMenuOpen((v) => !v)}
                      className="flex max-w-[10rem] items-center gap-1 rounded-md px-1.5 py-1 font-mono text-[11px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
                      title="切换模型与推理强度"
                      aria-label="切换模型与推理强度"
                      aria-expanded={modelMenuOpen}
                    >
                      <span className="min-w-0 flex-1 truncate">{modelTriggerLabel}</span>
                    </button>
                    {modelMenuOpen && (
                      <ModelMenu
                        models={panelModels}
                        value={selectedModel}
                        effort={effort}
                        onSelectModel={handlePanelModelSelect}
                        onSelectEffort={handleEffortSelect}
                        onClose={() => setModelMenuOpen(false)}
                      />
                    )}
                  </div>
                }
              />
            </div>
            {/* 2E：斜杠命令菜单挂在输入框 relative 容器内 */}
            <div className="relative">
              <div className="flex gap-2">
                <textarea
                  value={input}
                  onChange={(e) => handleInputChange(e.target.value)}
                  onKeyDown={handleKeyDown}
                  placeholder={inputPlaceholder}
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
                  <Button type="button" size="icon" className="size-9 shrink-0" onClick={() => handleSend()} disabled={!input.trim()} aria-label="发送">
                    <Send className="size-4" />
                  </Button>
                )}
              </div>
              {slashOpen && slashCommands.length > 0 && (
                <SlashMenu
                  ref={slashMenuRef}
                  commands={slashCommands}
                  query={slashQuery}
                  onSelect={(cmd) => handleSlashSelect(cmd)}
                  onClose={() => setSlashOpen(false)}
                />
              )}
            </div>
            {view.activeTurnId && <p className="mt-1.5 text-[11px] text-muted-foreground">Agent 正在执行… Enter 排队，⌘/Ctrl+Enter 立即插话，或按 Esc 中断</p>}
          </div>

          {/* 底部状态栏：模型 · token（title 含限流快照） · 回合状态 */}
          <div className="flex items-center gap-2 border-t border-border/50 bg-muted/30 px-2.5 py-1 font-mono text-[11px] text-muted-foreground">
            <span className="min-w-0 flex-1 truncate" title={selectedModel ?? undefined}>
              {selectedModel ?? "未选模型"}
            </span>
            {tokenText && (
              <>
                <span className="shrink-0 text-border">·</span>
                <span className="shrink-0 truncate" title={tokenTitle}>
                  {tokenText}
                </span>
              </>
            )}
            <span className="shrink-0 text-border">·</span>
            <span className={`shrink-0 ${view.activeTurnId ? "text-amber-600 dark:text-amber-400" : ""}`}>{turnStateLabel}</span>
          </div>

          {/* 2E：全屏 diff 审查（拒绝即文件级选择撤销；接受仅关闭） */}
          <DiffReviewDialog
            open={reviewTurnId !== null}
            turnId={reviewTurnId}
            diff={reviewDiff}
            threadId={view.threadId || null}
            vaultRoot={vaultRoot}
            onClose={closeReview}
            onFilesReverted={onFilesReverted}
            onThreadReverted={onThreadReverted}
          />
        </>
      )}

      {queue.length > 0 && (
        <ApprovalDialog queue={queue} view={view} onRespond={handleApprovalRespond} allowlist={allowlist} onAllowlistChanged={bumpAllowlist} />
      )}
    </div>
  );
}
