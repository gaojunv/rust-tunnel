import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { isTauri } from "@/api/tauri";
import { login, listKnowledgeSources, loadSyncConfig, saveSyncConfig, type KnowledgeSourceInfo } from "@/api/server";
import { getToken } from "@/lib/server-auth";
import { ServerError } from "@/api/server";
import type { AgentSettingsDto, AgentStatusDto, BinaryResolveDto } from "@/lib/agent/client";
import {
  getSettings,
  saveSettings,
  detectBinary,
  getStatus,
  start,
  stop,
  accountLoginStart,
  getAuthStatus,
  openExternal,
  onAgentNotification,
} from "@/lib/agent/client";
import { listAvailableModels, type ModelOption } from "@/lib/agent/models";
import {
  createInitialAuthFlowState,
  reduceAuthFlow,
  CHAT_LOGIN_POLL_INTERVAL_MS,
  CHAT_LOGIN_MAX_DURATION_MS,
} from "@/lib/agent/authflow";

/**
 * 同步设置对话框 —— 复用 NoteFormDialog 的模态范式
 * 字段：服务器地址 / 密码 / 知识容器下拉 / 传播删除开关
 * M3 新增：Agent (Codex) 分区
 */

type Props = {
  onClose: () => void;
  onSync: () => void;
};

function normalizeApprovalForUi(v: string): string {
  if (v === "auto") return "untrusted";
  return v;
}
function normalizeApprovalForWire(v: string): string {
  if (v === "untrusted") return "auto";
  return v;
}

export function SettingsDialog({ onClose, onSync }: Props) {
  // 默认 baseUrl：非 Tauri 用 mock://local，Tauri 为空
  const defaultBaseUrl = isTauri ? "" : "mock://local";
  const saved = loadSyncConfig();

  const [baseUrl, setBaseUrl] = useState(saved?.baseUrl ?? defaultBaseUrl);
  const [password, setPassword] = useState("");
  const [knowledgeId, setKnowledgeId] = useState(saved?.knowledgeId ?? "");
  const [propagateDeletes, setPropagateDeletes] = useState(saved?.propagateDeletes ?? false);
  const [autoSyncAfterSave, setAutoSyncAfterSave] = useState(saved?.autoSyncAfterSave ?? true);
  const [syncIntervalMinutes, setSyncIntervalMinutes] = useState(String(saved?.syncIntervalMinutes ?? 0));

  const [sources, setSources] = useState<KnowledgeSourceInfo[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [loggedIn, setLoggedIn] = useState(false);
  const [saving, setSaving] = useState(false);

  // —— Agent 侧状态 ——
  const syncBaseUrlForPlaceholder = (loadSyncConfig()?.baseUrl ?? baseUrl ?? "").trim();
  const gatewayPlaceholder = syncBaseUrlForPlaceholder || "https://example.com";

  const [agentSettings, setAgentSettings] = useState<AgentSettingsDto | null>(null);
  const [agentLoading, setAgentLoading] = useState(true);
  const [agentSaving, setAgentSaving] = useState(false);
  const [agentError, setAgentError] = useState<string | null>(null);
  const [agentSaved, setAgentSaved] = useState(false);
  const [agentStatus, setAgentStatus] = useState<AgentStatusDto | null>(null);
  const [showRestartPrompt, setShowRestartPrompt] = useState(false);
  const [restarting, setRestarting] = useState(false);
  const [initialAgentJson, setInitialAgentJson] = useState<string>("");

  // binary 检测
  const [binaryInfo, setBinaryInfo] = useState<BinaryResolveDto | null>(null);
  const [binaryChecked, setBinaryChecked] = useState(false);
  const [binaryDetecting, setBinaryDetecting] = useState(false);
  const [binaryError, setBinaryError] = useState<string | null>(null);

  // 模型
  const [models, setModels] = useState<ModelOption[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [modelsError, setModelsError] = useState<string | null>(null);
  const isAgentRunning = agentStatus?.phase === "running";

  // 高级折叠
  const [advancedOpen, setAdvancedOpen] = useState(false);

  // ChatGPT 登录
  const [chatLoginState, setChatLoginState] = useState(() => createInitialAuthFlowState());
  const [chatLoginBusy, setChatLoginBusy] = useState(false);
  const chatLoginRef = useRef(chatLoginState);
  useEffect(() => {
    chatLoginRef.current = chatLoginState;
  }, [chatLoginState]);
  const loginPollTimerRef = useRef<number | null>(null);
  const loginUnlistenRef = useRef<(() => void) | null>(null);
  const loginStartTimeRef = useRef<number>(0);

  const clearChatLoginPoll = useCallback(() => {
    if (loginPollTimerRef.current != null) {
      window.clearInterval(loginPollTimerRef.current);
      loginPollTimerRef.current = null;
    }
    if (loginUnlistenRef.current) {
      try {
        loginUnlistenRef.current();
      } catch {
        // ignore
      }
      loginUnlistenRef.current = null;
    }
  }, []);

  useEffect(() => {
    return () => {
      clearChatLoginPoll();
    };
  }, [clearChatLoginPoll]);

  // 尝试用已存 token 预拉容器列表
  useEffect(() => {
    const cfg = loadSyncConfig();
    if (!cfg?.baseUrl || !cfg?.knowledgeId) return;
    const token = getToken(cfg.baseUrl);
    if (!token) return;
    setLoggedIn(true);
    listKnowledgeSources(cfg.baseUrl)
      .then((list) => setSources(list))
      .catch(() => {
        // 忽略，等待用户重新登录
      });
  }, []);

  // 加载 Agent 设置与状态
  useEffect(() => {
    if (!isTauri) {
      setAgentLoading(false);
      return;
    }
    let cancelled = false;
    // getSettings 回填
    getSettings()
      .then((s) => {
        if (cancelled) return;
        // 归一化 approvalPolicy 供 UI 使用（auto -> untrusted）
        const normalized: AgentSettingsDto = {
          ...s,
          approvalPolicy: normalizeApprovalForUi(String(s.approvalPolicy ?? "on-request")),
        };
        setAgentSettings(normalized);
        setInitialAgentJson(JSON.stringify(normalized));
      })
      .catch((e: unknown) => {
        if (cancelled) return;
        const msg = e instanceof Error ? e.message : String(e);
        setAgentError(msg);
        // 兜底默认值
        const fallback: AgentSettingsDto = {
          enabled: false,
          authMode: "gateway",
          gatewayBaseUrl: null,
          gatewayApiKey: null,
          openaiApiKey: null,
          model: null,
          approvalPolicy: "on-request",
          sandboxMode: "workspace-write",
          codexPathOverride: null,
        };
        setAgentSettings(fallback);
        setInitialAgentJson(JSON.stringify(fallback));
      })
      .finally(() => {
        if (!cancelled) setAgentLoading(false);
      });

    getStatus()
      .then((st) => {
        if (!cancelled) setAgentStatus(st);
      })
      .catch(() => {
        // ignore
      });

    return () => {
      cancelled = true;
    };
  }, []);

  // 模型列表：gateway 模式不依赖 running，其余模式仅 running 时拉取
  const settingsAuthMode = String(agentSettings?.authMode ?? "gateway");
  useEffect(() => {
    if (!isTauri) return;
    const isGateway = settingsAuthMode === "gateway";
    if (!isGateway && !isAgentRunning) return;
    let cancelled = false;
    setModelsLoading(true);
    setModelsError(null);
    listAvailableModels(settingsAuthMode)
      .then((list) => {
        if (cancelled) return;
        setModels(list);
      })
      .catch((e: unknown) => {
        if (cancelled) return;
        const msg = e instanceof Error ? e.message : String(e);
        setModelsError(msg);
        setModels([]);
      })
      .finally(() => {
        if (!cancelled) setModelsLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [isAgentRunning, settingsAuthMode]);

  // ChatGPT：订阅 account/login/completed 提前结束轮询
  const setupLoginNotificationListener = useCallback(async () => {
    try {
      const unlisten = await onAgentNotification((n) => {
        const method = (n as unknown as { method: string }).method;
        if (method !== "account/login/completed") return;
        const params = (n as unknown as { params: unknown }).params as Record<string, unknown> | null | undefined;
        const success = Boolean(params?.["success"]);
        const error = (params?.["error"] as string | null | undefined) ?? null;
        // 复用 pure reducer
        setChatLoginState((prev) => {
          if (prev.status !== "pending") return prev;
          const next = reduceAuthFlow(prev, {
            type: "notification",
            notif: {
              loginId: (params?.["loginId"] as string | null) ?? prev.loginId ?? null,
              success,
              error,
              onboardingEntrypoint: null,
            } as unknown as Parameters<typeof reduceAuthFlow>[1] extends { type: "notification"; notif: infer N } ? N : never,
          });
          if (next.status !== "pending") {
            clearChatLoginPoll();
            setChatLoginBusy(false);
          }
          return next;
        });
      });
      loginUnlistenRef.current = unlisten;
    } catch {
      // ignore
    }
  }, [clearChatLoginPoll]);

  const handleChatLogin = useCallback(async () => {
    if (!isTauri) return;
    setAgentError(null);
    setChatLoginBusy(true);
    setChatLoginState(createInitialAuthFlowState());
    clearChatLoginPoll();
    loginStartTimeRef.current = Date.now();

    try {
      // 确保 agent running
      let st: AgentStatusDto | null = null;
      try {
        st = await getStatus();
        setAgentStatus(st);
      } catch {
        // ignore
      }
      if (!st || st.phase !== "running") {
        const started = await start();
        setAgentStatus(started);
      }

      // 订阅通知
      await setupLoginNotificationListener();

      const resp = await accountLoginStart({ type: "chatgpt" } as unknown as Parameters<typeof accountLoginStart>[0]);
      const r = resp as unknown as Record<string, unknown>;
      const authUrl = (r["authUrl"] as string | undefined) ?? (r["auth_url"] as string | undefined) ?? null;
      const loginId = (r["loginId"] as string | undefined) ?? (r["login_id"] as string | undefined) ?? null;
      if (!authUrl) {
        throw new Error("登录响应缺少 authUrl");
      }
      setChatLoginState((prev) => ({ ...prev, loginId: loginId ?? prev.loginId ?? null }));
      await openExternal(authUrl);

      // 轮询 getAuthStatus 每 2s，最多 2 分钟
      const timer = window.setInterval(async () => {
        const elapsed = Date.now() - loginStartTimeRef.current;
        if (elapsed >= CHAT_LOGIN_MAX_DURATION_MS) {
          setChatLoginState((prev) => {
            if (prev.status !== "pending") return prev;
            return reduceAuthFlow(prev, { type: "timeout" });
          });
          setChatLoginBusy(false);
          clearChatLoginPoll();
          return;
        }
        try {
          const status = await getAuthStatus({ includeToken: false, refreshToken: false });
          const nextPending = reduceAuthFlow(chatLoginRef.current, { type: "poll", resp: status });
          if (nextPending.status === "success") {
            setChatLoginState(nextPending);
            setChatLoginBusy(false);
            clearChatLoginPoll();
          }
        } catch (e: unknown) {
          const msg = e instanceof Error ? e.message : String(e);
          // 网络瞬断不立即失败，继续轮询；仅在超时或明确错误时才失败
          if (elapsed > CHAT_LOGIN_MAX_DURATION_MS - CHAT_LOGIN_POLL_INTERVAL_MS) {
            setChatLoginState((prev) => reduceAuthFlow(prev, { type: "error", error: msg }));
            setChatLoginBusy(false);
            clearChatLoginPoll();
          }
        }
      }, CHAT_LOGIN_POLL_INTERVAL_MS);
      loginPollTimerRef.current = timer;
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      setChatLoginState((prev) => reduceAuthFlow(prev, { type: "error", error: msg }));
      setChatLoginBusy(false);
      clearChatLoginPoll();
    }
  }, [clearChatLoginPoll, setupLoginNotificationListener]);

  // Esc 关闭
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const handleLogin = useCallback(async () => {
    const url = baseUrl.trim();
    if (!url) {
      setError("请输入服务器地址");
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const res = await login(url, password);
      // 登录成功后拉容器列表
      void res;
      setLoggedIn(true);
      const list = await listKnowledgeSources(url);
      setSources(list);
      // 若当前未选容器且列表非空，默认选中第一项
      if (!knowledgeId && list.length > 0) {
        setKnowledgeId(list[0].id);
      } else if (knowledgeId && !list.some((s) => s.id === knowledgeId) && list.length > 0) {
        // 已选 id 不在列表中，保持原值（允许用户手动选），不自动覆盖
      }
      setError(null);
    } catch (e: unknown) {
      if (e instanceof ServerError && e.status === 401) {
        setError("密码错误");
      } else {
        const msg = e instanceof Error ? e.message : String(e);
        setError(msg);
      }
      setLoggedIn(false);
    } finally {
      setLoading(false);
    }
  }, [baseUrl, password, knowledgeId]);

  const handleSave = useCallback(() => {
    const url = baseUrl.trim();
    if (!url) {
      setError("请输入服务器地址");
      return;
    }
    if (!knowledgeId) {
      setError("请选择知识容器");
      return;
    }
    const mins = Math.max(0, Math.floor(Number(syncIntervalMinutes) || 0));
    setSaving(true);
    try {
      saveSyncConfig({ baseUrl: url, knowledgeId, propagateDeletes, autoSyncAfterSave, syncIntervalMinutes: mins });
      setSaving(false);
      onClose();
    } catch (e: unknown) {
      setSaving(false);
      const msg = e instanceof Error ? e.message : String(e);
      setError(msg);
    }
  }, [baseUrl, knowledgeId, propagateDeletes, autoSyncAfterSave, syncIntervalMinutes, onClose]);

  const handleSyncClick = useCallback(() => {
    const url = baseUrl.trim();
    if (!url || !knowledgeId) {
      setError("请先完成服务器地址与知识容器配置并登录");
      return;
    }
    const mins = Math.max(0, Math.floor(Number(syncIntervalMinutes) || 0));
    // 保存配置后触发同步
    try {
      saveSyncConfig({ baseUrl: url, knowledgeId, propagateDeletes, autoSyncAfterSave, syncIntervalMinutes: mins });
    } catch {
      // 忽略存储异常
    }
    onClose();
    onSync();
  }, [baseUrl, knowledgeId, propagateDeletes, autoSyncAfterSave, syncIntervalMinutes, onClose, onSync]);

  const canSync = loggedIn && !!knowledgeId && !loading;

  // —— Agent 辅助 ——
  const updateAgent = useCallback(
    (patch: Partial<AgentSettingsDto>) => {
      setAgentSettings((prev) => {
        if (!prev) return prev;
        return { ...prev, ...patch };
      });
      setAgentSaved(false);
      setShowRestartPrompt(false);
    },
    [],
  );

  const handleDetectBinary = useCallback(async () => {
    if (!isTauri) return;
    setBinaryDetecting(true);
    setBinaryError(null);
    try {
      const res = await detectBinary();
      setBinaryInfo(res);
      setBinaryChecked(true);
      if (res?.path) {
        // 不自动覆盖用户已填的覆盖路径，仅展示信息
      }
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      setBinaryError(msg);
      setBinaryInfo(null);
      setBinaryChecked(true);
    } finally {
      setBinaryDetecting(false);
    }
  }, []);

  const handleAgentSave = useCallback(async () => {
    if (!isTauri || !agentSettings) return;
    setAgentSaving(true);
    setAgentError(null);
    try {
      const toSave: AgentSettingsDto = {
        ...agentSettings,
        approvalPolicy: normalizeApprovalForWire(String(agentSettings.approvalPolicy)),
      };
      await saveSettings(toSave);
      setAgentSaved(true);
      const currentJson = JSON.stringify({ ...agentSettings, approvalPolicy: normalizeApprovalForWire(String(agentSettings.approvalPolicy)) });
      const changed = currentJson !== initialAgentJson;
      setInitialAgentJson(currentJson);
      // 归一化后同步回 UI 的显示值
      setAgentSettings((prev) => (prev ? { ...prev, approvalPolicy: normalizeApprovalForUi(String(toSave.approvalPolicy)) } : prev));
      if (changed && agentStatus?.phase === "running") {
        setShowRestartPrompt(true);
      } else {
        setShowRestartPrompt(false);
      }
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      setAgentError(msg);
    } finally {
      setAgentSaving(false);
    }
  }, [agentSettings, initialAgentJson, agentStatus]);

  const handleRestartAgent = useCallback(async () => {
    if (!isTauri) return;
    setRestarting(true);
    setAgentError(null);
    try {
      await stop();
      const st = await start();
      setAgentStatus(st);
      setShowRestartPrompt(false);
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      setAgentError(msg);
    } finally {
      setRestarting(false);
    }
  }, []);

  const chatStatusText = (() => {
    switch (chatLoginState.status) {
      case "pending":
        return chatLoginBusy ? "登录中…请在浏览器完成授权" : "未登录";
      case "success":
        return `已登录${chatLoginState.loginId ? ` ${chatLoginState.loginId}` : ""}`;
      case "failed":
        return `登录失败${chatLoginState.error ? `：${chatLoginState.error}` : ""}`;
      case "timeout":
        return "登录超时，请重试";
      default:
        return "未登录";
    }
  })();

  const overlay = (
    <div
      data-modal-open=""
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="flex max-h-[90vh] w-[min(92vw,560px)] flex-col rounded-lg border border-border bg-popover shadow-xl"
        onMouseDown={(e) => e.stopPropagation()}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="shrink-0 border-b px-4 py-3">
          <h2 className="text-sm font-semibold">设置</h2>
          <p className="mt-1 text-xs text-muted-foreground">配置同步与本地 Agent</p>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4">
          {/* 同步设置分区 */}
          <div className="space-y-3">
            <h3 className="text-xs font-semibold tracking-wide text-muted-foreground">同步设置</h3>
            <p className="text-xs text-muted-foreground">配置服务端同步的知识容器与连接信息</p>
          <div>
            <label className="block text-xs font-medium text-muted-foreground">服务器地址</label>
            <Input
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
              placeholder={isTauri ? "https://example.com" : "mock://local"}
              className="mt-1.5"
            />
          </div>

          <div>
            <label className="block text-xs font-medium text-muted-foreground">密码</label>
            <Input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder={isTauri ? "请输入密码" : "mock（演示环境）"}
              className="mt-1.5"
            />
            <p className="mt-1 text-xs text-muted-foreground">仅用于登录，不会被持久化</p>
          </div>

          <div className="flex items-center gap-2">
            <Button type="button" onClick={() => void handleLogin()} disabled={loading || !baseUrl.trim()}>
              {loading ? "连接中…" : "连接并登录"}
            </Button>
            {loggedIn && <span className="text-xs text-green-600">已登录</span>}
          </div>

          {error && <p className="text-xs text-destructive">{error}</p>}

          <div>
            <label className="block text-xs font-medium text-muted-foreground">知识容器</label>
            <select
              value={knowledgeId}
              onChange={(e) => setKnowledgeId(e.target.value)}
              className="mt-1.5 flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus:outline-none focus:ring-2 focus:ring-ring focus:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50"
            >
              <option value="">{sources.length === 0 ? "请先登录以加载容器" : "请选择容器"}</option>
              {sources.map((s) => (
                <option key={s.id} value={s.id}>
                  {s.name} ({s.id})
                </option>
              ))}
            </select>
          </div>

          <label className="flex items-start gap-2 rounded-md border p-3">
            <input
              type="checkbox"
              checked={propagateDeletes}
              onChange={(e) => setPropagateDeletes(e.target.checked)}
              className="mt-0.5"
            />
            <span className="flex-1">
              <span className="block text-xs font-medium">传播删除</span>
              <span className="block text-xs text-muted-foreground">开启后，本地删除笔记会同步删除服务端页面</span>
            </span>
          </label>

          <label className="flex items-start gap-2 rounded-md border p-3">
            <input
              type="checkbox"
              checked={autoSyncAfterSave}
              onChange={(e) => setAutoSyncAfterSave(e.target.checked)}
              className="mt-0.5"
            />
            <span className="flex-1">
              <span className="block text-xs font-medium">保存后自动同步</span>
              <span className="block text-xs text-muted-foreground">开启后，保存笔记 30 秒后自动同步</span>
            </span>
          </label>

          <div>
            <label className="block text-xs font-medium text-muted-foreground">定时同步间隔（分钟，0 为关闭）</label>
            <Input
              type="number"
              min={0}
              step={1}
              value={syncIntervalMinutes}
              onChange={(e) => setSyncIntervalMinutes(e.target.value)}
              placeholder="0"
              className="mt-1.5"
            />
          </div>
          </div>

          {/* Agent (Codex) 分区 */}
          <div className="mt-6 space-y-3 border-t pt-6">
            <h3 className="text-xs font-semibold tracking-wide text-muted-foreground">Agent (Codex)</h3>
            {!isTauri ? (
              <p className="text-xs text-muted-foreground">Agent 仅在桌面端可用</p>
            ) : agentLoading || !agentSettings ? (
              <p className="text-xs text-muted-foreground">加载 Agent 配置…</p>
            ) : (
              <>
                <label className="flex items-center justify-between rounded-md border p-3">
                  <span className="text-xs font-medium">启用 Agent</span>
                  <input
                    type="checkbox"
                    checked={Boolean(agentSettings.enabled)}
                    onChange={(e) => updateAgent({ enabled: e.target.checked })}
                    className="size-4"
                  />
                </label>

                <div>
                  <span className="block text-xs font-medium text-muted-foreground">认证模式</span>
                  <div className="mt-1.5 flex flex-col gap-2">
                    <label className="flex items-center gap-2 text-xs">
                      <input
                        type="radio"
                        name="authMode"
                        value="gateway"
                        checked={String(agentSettings.authMode) === "gateway"}
                        onChange={() => updateAgent({ authMode: "gateway" })}
                      />
                      Tunnel 网关（推荐）
                    </label>
                    <label className="flex items-center gap-2 text-xs">
                      <input
                        type="radio"
                        name="authMode"
                        value="openai-key"
                        checked={String(agentSettings.authMode) === "openai-key"}
                        onChange={() => updateAgent({ authMode: "openai-key" })}
                      />
                      OpenAI API Key
                    </label>
                    <label className="flex items-center gap-2 text-xs">
                      <input
                        type="radio"
                        name="authMode"
                        value="chatgpt"
                        checked={String(agentSettings.authMode) === "chatgpt"}
                        onChange={() => updateAgent({ authMode: "chatgpt" })}
                      />
                      ChatGPT 账号
                    </label>
                  </div>
                </div>

                {String(agentSettings.authMode) === "gateway" && (
                  <>
                    <div>
                      <label className="block text-xs font-medium text-muted-foreground">网关地址</label>
                      <Input
                        value={String(agentSettings.gatewayBaseUrl ?? "")}
                        onChange={(e) => updateAgent({ gatewayBaseUrl: e.target.value })}
                        placeholder={gatewayPlaceholder}
                        className="mt-1.5"
                      />
                      <p className="mt-1 text-xs text-muted-foreground">需服务端 LLM 网关暴露 /v1/responses，默认取同步服务器地址</p>
                    </div>
                    <div>
                      <label className="block text-xs font-medium text-muted-foreground">网关 API Key</label>
                      <Input
                        type="password"
                        value={String(agentSettings.gatewayApiKey ?? "")}
                        onChange={(e) => updateAgent({ gatewayApiKey: e.target.value })}
                        placeholder="sk-..."
                        className="mt-1.5"
                      />
                      <p className="mt-1 text-xs text-muted-foreground">在服务端「LLM 网关 → API Keys」页签发</p>
                    </div>
                  </>
                )}

                {String(agentSettings.authMode) === "openai-key" && (
                  <div>
                    <label className="block text-xs font-medium text-muted-foreground">OpenAI API Key</label>
                    <Input
                      type="password"
                      value={String(agentSettings.openaiApiKey ?? "")}
                      onChange={(e) => updateAgent({ openaiApiKey: e.target.value })}
                      placeholder="sk-..."
                      className="mt-1.5"
                    />
                  </div>
                )}

                {String(agentSettings.authMode) === "chatgpt" && (
                  <div className="rounded-md border p-3">
                    <div className="flex items-center gap-2">
                      <Button type="button" size="sm" onClick={() => void handleChatLogin()} disabled={chatLoginBusy}>
                        {chatLoginBusy ? "登录中…" : "ChatGPT 登录"}
                      </Button>
                      <span className="text-xs text-muted-foreground">{chatStatusText}</span>
                    </div>
                    {chatLoginState.error && <p className="mt-2 text-xs text-destructive">{chatLoginState.error}</p>}
                    <p className="mt-2 text-xs text-muted-foreground">将打开浏览器完成 OAuth，完成后自动轮询 2 分钟</p>
                  </div>
                )}

                <div>
                  <label className="block text-xs font-medium text-muted-foreground">模型</label>
                  {settingsAuthMode === "gateway" ? (
                    <>
                      {modelsLoading ? (
                        <p className="mt-1.5 text-xs text-muted-foreground">加载模型列表…</p>
                      ) : (
                        <select
                          value={String(agentSettings.model ?? "")}
                          onChange={(e) => updateAgent({ model: e.target.value || null })}
                          className="mt-1.5 flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus:outline-none focus:ring-2 focus:ring-ring focus:ring-offset-2"
                        >
                          <option value="">{models.length === 0 ? "无模型" : "默认模型"}</option>
                          {models.map((m) => (
                            <option key={m.id} value={m.id}>
                              {m.label}
                            </option>
                          ))}
                        </select>
                      )}
                      {modelsError && <p className="mt-1.5 text-xs text-destructive">加载失败：{modelsError}</p>}
                    </>
                  ) : isAgentRunning ? (
                    <>
                      {modelsLoading ? (
                        <p className="mt-1.5 text-xs text-muted-foreground">加载模型列表…</p>
                      ) : models.length > 0 ? (
                        <select
                          value={String(agentSettings.model ?? "")}
                          onChange={(e) => updateAgent({ model: e.target.value || null })}
                          className="mt-1.5 flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus:outline-none focus:ring-2 focus:ring-ring focus:ring-offset-2"
                        >
                          <option value="">默认模型</option>
                          {models.map((m) => (
                            <option key={m.id} value={m.id}>
                              {m.label}
                            </option>
                          ))}
                        </select>
                      ) : (
                        <>
                          {modelsError && <p className="mt-1.5 text-xs text-destructive">加载失败：{modelsError}</p>}
                          <Input
                            value={String(agentSettings.model ?? "")}
                            onChange={(e) => updateAgent({ model: e.target.value })}
                            placeholder="如 gpt-4o"
                            className="mt-1.5"
                          />
                        </>
                      )}
                    </>
                  ) : (
                    <>
                      <p className="mt-1 text-xs text-muted-foreground">启动 Agent 后可拉取模型列表</p>
                      <Input
                        value={String(agentSettings.model ?? "")}
                        onChange={(e) => updateAgent({ model: e.target.value })}
                        placeholder="如 gpt-4o"
                        className="mt-1.5"
                      />
                    </>
                  )}
                </div>

                <div>
                  <label className="block text-xs font-medium text-muted-foreground">审批策略</label>
                  <select
                    value={normalizeApprovalForUi(String(agentSettings.approvalPolicy ?? "on-request"))}
                    onChange={(e) => updateAgent({ approvalPolicy: e.target.value })}
                    className="mt-1.5 flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus:outline-none focus:ring-2 focus:ring-ring focus:ring-offset-2"
                  >
                    <option value="on-request">每次请求</option>
                    <option value="untrusted">仅危险命令</option>
                    <option value="never">从不（自动批准全部，危险）</option>
                  </select>
                </div>

                <div>
                  <label className="block text-xs font-medium text-muted-foreground">沙箱模式</label>
                  <select
                    value={String(agentSettings.sandboxMode ?? "workspace-write")}
                    onChange={(e) => updateAgent({ sandboxMode: e.target.value })}
                    className="mt-1.5 flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus:outline-none focus:ring-2 focus:ring-ring focus:ring-offset-2"
                  >
                    <option value="workspace-write">workspace-write（默认）</option>
                    <option value="read-only">read-only</option>
                  </select>
                  <div className="mt-2 rounded-md border">
                    <button
                      type="button"
                      onClick={() => setAdvancedOpen((v) => !v)}
                      className="flex w-full items-center justify-between px-3 py-2 text-xs font-medium"
                    >
                      高级
                      <span className="text-muted-foreground">{advancedOpen ? "收起" : "展开"}</span>
                    </button>
                    {advancedOpen && (
                      <div className="border-t p-3">
                        <label className="flex items-center gap-2 text-xs">
                          <input
                            type="radio"
                            name="sandboxAdvanced"
                            checked={String(agentSettings.sandboxMode) === "danger-full-access"}
                            onChange={() => updateAgent({ sandboxMode: "danger-full-access" })}
                          />
                          danger-full-access
                        </label>
                        <p className="mt-2 rounded bg-destructive/10 px-2 py-1 text-xs text-destructive">危险：允许完全访问文件系统与网络，请仅在受控环境使用</p>
                        {String(agentSettings.sandboxMode) === "danger-full-access" && (
                          <Button
                            type="button"
                            variant="outline"
                            size="sm"
                            className="mt-2"
                            onClick={() => updateAgent({ sandboxMode: "workspace-write" })}
                          >
                            切回 workspace-write
                          </Button>
                        )}
                      </div>
                    )}
                  </div>
                </div>

                <div>
                  <label className="block text-xs font-medium text-muted-foreground">codex 路径覆盖</label>
                  <div className="mt-1.5 flex gap-2">
                    <Input
                      value={String(agentSettings.codexPathOverride ?? "")}
                      onChange={(e) => updateAgent({ codexPathOverride: e.target.value })}
                      placeholder="/usr/local/bin/codex"
                      className="flex-1"
                    />
                    <Button type="button" variant="outline" onClick={() => void handleDetectBinary()} disabled={binaryDetecting}>
                      {binaryDetecting ? "检测中…" : "自动检测"}
                    </Button>
                  </div>
                  {binaryChecked && (
                    <div className="mt-2 rounded-md border bg-muted/30 px-3 py-2 text-xs">
                      {binaryInfo ? (
                        <>
                          <div>路径：{binaryInfo.path}</div>
                          <div>来源：{binaryInfo.source}</div>
                          {binaryInfo.version && <div>版本：{binaryInfo.version}</div>}
                        </>
                      ) : (
                        <span>未检测到 codex 二进制，可通过 npm i -g @openai/codex 安装</span>
                      )}
                      {binaryError && <div className="mt-1 text-destructive">{binaryError}</div>}
                    </div>
                  )}
                </div>

                {agentError && <p className="text-xs text-destructive">{agentError}</p>}
                {agentSaved && !agentError && <p className="text-xs text-green-600">已保存</p>}
                {showRestartPrompt && (
                  <div className="rounded-md border border-amber-200 bg-amber-50 p-3 dark:border-amber-900 dark:bg-amber-950">
                    <p className="text-xs text-amber-800 dark:text-amber-200">配置已变更，需重启 Agent 生效</p>
                    <Button type="button" size="sm" className="mt-2" onClick={() => void handleRestartAgent()} disabled={restarting}>
                      {restarting ? "重启中…" : "重启 Agent 生效"}
                    </Button>
                  </div>
                )}

                <div className="flex gap-2">
                  <Button type="button" onClick={() => void handleAgentSave()} disabled={agentSaving}>
                    {agentSaving ? "保存中…" : "保存 Agent 设置"}
                  </Button>
                </div>
              </>
            )}
          </div>
        </div>

        <div className="flex shrink-0 justify-between gap-2 border-t px-4 py-3">
          <Button type="button" variant="ghost" onClick={onClose} disabled={saving || agentSaving}>
            取消
          </Button>
          <div className="flex gap-2">
            <Button type="button" variant="outline" onClick={handleSave} disabled={saving}>
              {saving ? "保存中…" : "保存"}
            </Button>
            <Button type="button" onClick={handleSyncClick} disabled={!canSync}>
              立即同步
            </Button>
          </div>
        </div>
      </div>
    </div>
  );

  return createPortal(overlay, document.body);
}
