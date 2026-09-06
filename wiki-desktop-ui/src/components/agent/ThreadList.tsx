/**
 * ThreadList —— thread 列表 + 活跃会话视图切换 + 模型选择
 * 线上以本地 store 关联元数据过滤当前 vault，叠加 codex 侧 thread/list 拉取
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { Button } from "@/components/ui/button";
import { modelList, threadArchive, threadList, threadResume, threadRead } from "@/lib/agent/client";
import { listThreads, archiveThread } from "@/lib/agent/store";
import type { AgentThreadRecord } from "@/lib/agent/store";
import type { Model } from "@/lib/agent/types/v2/Model";
import type { Thread } from "@/lib/agent/types/v2/Thread";

type Props = {
  vaultRoot: string | null;
  activeThreadId: string | null;
  isRunning: boolean;
  onSelectThread: (threadId: string, hydrated: { thread: Thread } | null) => void;
  onNewThread: () => void;
  onArchived: (threadId: string) => void;
  model: string | null;
  onModelChange: (model: string | null) => void;
};

function formatTime(ts: number): string {
  try {
    const d = new Date(ts * 1000);
    return d.toLocaleString();
  } catch {
    return String(ts);
  }
}

export function ThreadList({
  vaultRoot,
  activeThreadId,
  isRunning,
  onSelectThread,
  onNewThread,
  onArchived,
  model,
  onModelChange,
}: Props) {
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [remoteThreads, setRemoteThreads] = useState<Thread[]>([]);
  const [models, setModels] = useState<Model[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [localVersion, setLocalVersion] = useState(0);

  const localThreads = useMemo<AgentThreadRecord[]>(() => {
    void localVersion;
    if (!vaultRoot) return [];
    try {
      const list = listThreads(vaultRoot);
      // 过滤已归档
      return list.filter((r) => !r.archived);
    } catch {
      return [];
    }
  }, [vaultRoot, localVersion]);

  const localIds = useMemo(() => new Set(localThreads.map((r) => r.threadId)), [localThreads]);

  // 按本地关联过滤 codex 侧历史（当前 vault）；若本地无关联则展示全部未归档线程的近期 20 条
  const filteredRemote = useMemo(() => {
    if (localIds.size === 0) return remoteThreads.slice(0, 20);
    return remoteThreads.filter((t) => localIds.has(t.id));
  }, [remoteThreads, localIds]);

  const refreshRemote = useCallback(async () => {
    if (!isRunning) return;
    setLoading(true);
    setError(null);
    try {
      const res = await threadList({ archived: false, limit: 50 });
      setRemoteThreads(res.data ?? []);
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [isRunning]);

  const refreshModels = useCallback(async () => {
    if (!isRunning) return;
    setModelsLoading(true);
    try {
      const res = await modelList({});
      setModels(res.data ?? []);
    } catch {
      // 忽略，保留空列表
    } finally {
      setModelsLoading(false);
    }
  }, [isRunning]);

  useEffect(() => {
    void refreshRemote();
    void refreshModels();
  }, [refreshRemote, refreshModels]);

  const handleResume = useCallback(
    async (threadId: string) => {
      try {
        // 优先用 threadResume 重建会话；若失败则直接本地切 threadId
        // resume 后尝试 threadRead 回填 items：若结构复杂则不回填历史仅续聊
        let hydrated: { thread: Thread } | null = null;
        try {
          // 复用 vaultRoot 关联的 model（若本地有记录）
          const preferredModel = vaultRoot
            ? (() => {
                try {
                  const rec = localThreads.find((r) => r.threadId === threadId);
                  return rec?.model ?? model ?? null;
                } catch {
                  return model ?? null;
                }
              })()
            : model ?? null;
          const resumeRes = await threadResume({
            threadId,
            ...(preferredModel ? { model: preferredModel } : {}),
          });
          // resume 的 thread 带 turns（若 excludeTurns=false 默认带），取 resumeRes.thread
          const thread = (resumeRes as unknown as { thread: Thread }).thread;
          if (thread) {
            hydrated = { thread };
          } else {
            // 尝试 threadRead 兜底
            try {
              const readRes = await threadRead({ threadId, includeTurns: true });
              hydrated = { thread: readRes.thread };
            } catch {
              // 回填失败：简化为 resume 后不回填历史只续聊
              hydrated = null;
            }
          }
        } catch {
          // resume 失败：仍允许本地切 threadId（由 AgentPanel 接管 threadId 切换与 codec 重建）
          hydrated = null;
        }
        onSelectThread(threadId, hydrated);
      } catch (e: unknown) {
        setError(e instanceof Error ? e.message : String(e));
      }
    },
    [onSelectThread, vaultRoot, localThreads, model],
  );

  const handleArchive = useCallback(
    async (threadId: string) => {
      const ok = window.confirm("归档该会话？归档后可在 codex 侧 thread/list(archived) 中找回。");
      if (!ok) return;
      try {
        await threadArchive({ threadId });
      } catch {
        // 即便远端失败也清理本地关联，避免幽灵条目
      }
      if (vaultRoot) {
        try {
          archiveThread(vaultRoot, threadId);
        } catch {
          // ignore
        }
      }
      setLocalVersion((n) => n + 1);
      // 本地过滤会隐藏；同时从 remote 列表移除
      setRemoteThreads((prev) => prev.filter((t) => t.id !== threadId));
      onArchived(threadId);
    },
    [vaultRoot, onArchived],
  );

  return (
    <div className="flex h-full flex-col gap-3 p-3">
      {/* 顶部：模型选择 + 新建 */}
      <div className="flex items-center gap-2">
        <select
          value={model ?? ""}
          onChange={(e) => onModelChange(e.target.value || null)}
          className="h-8 min-w-0 flex-1 rounded-md border border-input bg-background px-2 text-xs"
          disabled={modelsLoading || !isRunning}
        >
          <option value="">{modelsLoading ? "加载模型…" : models.length === 0 ? "无模型" : "选择模型"}</option>
          {models.map((m) => (
            <option key={m.id} value={m.model || m.id}>
              {m.displayName ? `${m.displayName} (${m.model || m.id})` : m.model || m.id}
            </option>
          ))}
        </select>
        <Button type="button" size="sm" onClick={onNewThread} disabled={!isRunning}>
          新建会话
        </Button>
      </div>

      <div className="flex items-center justify-between">
        <p className="text-xs font-medium">会话列表</p>
        <Button type="button" variant="ghost" size="sm" className="h-7 text-xs" onClick={() => void refreshRemote()} disabled={loading || !isRunning}>
          刷新
        </Button>
      </div>

      {error && <p className="rounded bg-destructive/10 px-2 py-1 text-xs text-destructive">{error}</p>}
      {!isRunning && <p className="text-xs text-muted-foreground">Agent 未运行，启动后可查看会话。</p>}

      <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-auto">
        {/* 本地关联区 */}
        <div>
          <p className="mb-1 text-[11px] font-medium text-muted-foreground">本 vault 会话（{localThreads.length}）</p>
          {localThreads.length === 0 ? (
            <p className="text-xs text-muted-foreground">暂无本 vault 会话，发送消息将创建。</p>
          ) : (
            <ul className="space-y-1">
              {localThreads.map((r) => (
                <li
                  key={r.threadId}
                  className={`flex items-center gap-2 rounded border px-2 py-1.5 ${activeThreadId === r.threadId ? "border-primary bg-accent/40" : "bg-card"}`}
                >
                  <button
                    type="button"
                    onClick={() => void handleResume(r.threadId)}
                    className="min-w-0 flex-1 text-left"
                    title={r.threadId}
                  >
                    <p className="truncate text-xs font-medium">{r.title || r.threadId.slice(0, 8)}</p>
                    <p className="truncate text-[11px] text-muted-foreground">
                      {formatTime(r.createdAt)} {r.model ? `· ${r.model}` : ""}
                    </p>
                  </button>
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    className="h-6 shrink-0 px-1.5 text-[11px]"
                    onClick={() => void handleArchive(r.threadId)}
                  >
                    归档
                  </Button>
                </li>
              ))}
            </ul>
          )}
        </div>

        {/* Codex 侧历史（按本地关联过滤） */}
        <div>
          <p className="mb-1 text-[11px] font-medium text-muted-foreground">
            Codex 侧历史{localIds.size > 0 ? "（已按本 vault 过滤）" : ""}（{filteredRemote.length}）
          </p>
          {isRunning && loading && <p className="text-xs text-muted-foreground">加载中…</p>}
          {!loading && filteredRemote.length === 0 && isRunning && (
            <p className="text-xs text-muted-foreground">暂无匹配会话。</p>
          )}
          <ul className="space-y-1">
            {filteredRemote.map((t) => {
              const localTitle = localThreads.find((r) => r.threadId === t.id)?.title;
              return (
                <li
                  key={t.id}
                  className={`flex items-center gap-2 rounded border px-2 py-1.5 ${activeThreadId === t.id ? "border-primary bg-accent/40" : "bg-card"}`}
                >
                  <button
                    type="button"
                    onClick={() => void handleResume(t.id)}
                    className="min-w-0 flex-1 text-left"
                    title={t.id}
                  >
                    <p className="truncate text-xs">{localTitle || t.preview || t.name || t.id.slice(0, 8)}</p>
                    <p className="truncate text-[11px] text-muted-foreground">
                      {formatTime(t.updatedAt)} {t.model ? `· ${t.model}` : ""} {t.status ? `· ${t.status}` : ""}
                    </p>
                  </button>
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    className="h-6 shrink-0 px-1.5 text-[11px]"
                    onClick={() => void handleArchive(t.id)}
                  >
                    归档
                  </Button>
                </li>
              );
            })}
          </ul>
        </div>
      </div>

      <p className="text-[11px] text-muted-foreground">
        回填策略：resume 优先，失败则仅切本地 threadId 续聊；threadRead items 结构复杂时简化为不回填历史，留注释续聊。
      </p>
    </div>
  );
}
