/**
 * ThreadList —— thread 列表 + 活跃会话视图切换 + 模型选择
 * 线上以本地 store 关联元数据过滤当前 vault，叠加 codex 侧 thread/list 拉取
 * 2C：handleResume 改为 threadTurnsList 历史回填（threadRead includeTurns 兜底）；
 *     删除按钮 + 已归档折叠区（恢复/删除）；删除的是当前会话时经 onDeleted 通知父级。
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  threadArchive,
  threadList,
  threadRead,
  threadResume,
  threadTurnsList,
  threadDelete,
  threadUnarchive,
} from "@/lib/agent/client";
import { listThreads, archiveThread, unarchiveThread, removeThread } from "@/lib/agent/store";
import type { AgentThreadRecord } from "@/lib/agent/store";
import { hydrateTurnsToItems } from "@/lib/agent/codec";
import type { ItemView } from "@/lib/agent/codec";
import type { Thread } from "@/lib/agent/types/v2/Thread";
import { listAvailableModels, type ModelOption } from "@/lib/agent/models";

/** resume 结果：历史回填 items（只读视图，可能为空）+ 会话显示名（可为 null） */
export type ThreadResumeResult = {
  items: ItemView[];
  title: string | null;
};

type Props = {
  vaultRoot: string | null;
  activeThreadId: string | null;
  isRunning: boolean;
  authMode: string;
  onSelectThread: (threadId: string, resumed: ThreadResumeResult | null) => void;
  onNewThread: () => void;
  onArchived: (threadId: string) => void;
  /** 删除的是当前会话时由父级切回列表/新建态 */
  onDeleted: (threadId: string) => void;
  model: string | null;
  onModelChange: (model: string | null) => void;
  onOpenSettings?: () => void;
};

function formatTime(ts: number): string {
  try {
    const d = new Date(ts * 1000);
    return d.toLocaleString();
  } catch {
    return String(ts);
  }
}

/** 行尾操作：主操作（归档/恢复）+ hover 显示的删除 */
function RowActions({
  primaryLabel,
  primaryTitle,
  onPrimary,
  onDelete,
  deleteTitle,
}: {
  primaryLabel: string;
  primaryTitle: string;
  onPrimary: () => void;
  onDelete: () => void;
  deleteTitle: string;
}) {
  return (
    <span className="flex shrink-0 items-center gap-0.5">
      <Button
        type="button"
        variant="ghost"
        size="sm"
        className="h-6 shrink-0 px-1.5 text-[11px]"
        onClick={onPrimary}
        title={primaryTitle}
      >
        {primaryLabel}
      </Button>
      <Button
        type="button"
        variant="ghost"
        size="sm"
        className="h-6 w-6 shrink-0 px-0 text-muted-foreground opacity-0 transition-opacity hover:bg-destructive/10 hover:text-destructive focus-visible:opacity-100 group-hover:opacity-100"
        onClick={onDelete}
        aria-label={deleteTitle}
        title={deleteTitle}
      >
        <Trash2 className="size-3.5" />
      </Button>
    </span>
  );
}

export function ThreadList({
  vaultRoot,
  activeThreadId,
  isRunning,
  authMode,
  onSelectThread,
  onNewThread,
  onArchived,
  onDeleted,
  model,
  onModelChange,
  onOpenSettings,
}: Props) {
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [remoteThreads, setRemoteThreads] = useState<Thread[]>([]);
  const [archivedThreads, setArchivedThreads] = useState<Thread[]>([]);
  const [archivedOpen, setArchivedOpen] = useState(false);
  const [models, setModels] = useState<ModelOption[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [modelsError, setModelsError] = useState<string | null>(null);
  const [localVersion, setLocalVersion] = useState(0);

  // 全部本地关联记录（含归档；是否归档见 archived 字段）
  const localThreads = useMemo<AgentThreadRecord[]>(() => {
    void localVersion;
    if (!vaultRoot) return [];
    try {
      return listThreads(vaultRoot);
    } catch {
      return [];
    }
  }, [vaultRoot, localVersion]);

  const localActive = useMemo(() => localThreads.filter((r) => !r.archived), [localThreads]);
  const localArchived = useMemo(() => localThreads.filter((r) => r.archived), [localThreads]);
  const localIds = useMemo(() => new Set(localActive.map((r) => r.threadId)), [localActive]);
  const archivedLocalIds = useMemo(
    () => new Set(localArchived.map((r) => r.threadId)),
    [localArchived],
  );

  // 按本地关联过滤 codex 侧历史（当前 vault）；若本地无关联则展示全部未归档线程的近期 20 条
  const filteredRemote = useMemo(() => {
    if (localIds.size === 0) return remoteThreads.slice(0, 20);
    return remoteThreads.filter((t) => localIds.has(t.id));
  }, [remoteThreads, localIds]);

  // 已归档区同样按本地关联过滤；无本地关联时展示近期 20 条（与活跃列表策略一致）
  const filteredArchivedRemote = useMemo(() => {
    if (archivedLocalIds.size === 0) return archivedThreads.slice(0, 20);
    return archivedThreads.filter((t) => archivedLocalIds.has(t.id));
  }, [archivedThreads, archivedLocalIds]);

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

  const refreshArchived = useCallback(async () => {
    if (!isRunning) return;
    try {
      const res = await threadList({ archived: true, limit: 50 });
      setArchivedThreads(res.data ?? []);
    } catch {
      // 已归档区拉取失败不整页报错，折叠区保持为空即可
    }
  }, [isRunning]);

  const refreshModels = useCallback(async () => {
    const isGateway = authMode === "gateway";
    if (!isGateway && !isRunning) return;
    setModelsLoading(true);
    setModelsError(null);
    try {
      const list = await listAvailableModels(authMode);
      setModels(list);
    } catch (e: unknown) {
      setModelsError(e instanceof Error ? e.message : String(e));
    } finally {
      setModelsLoading(false);
    }
  }, [authMode, isRunning]);

  useEffect(() => {
    void refreshRemote();
    void refreshArchived();
    void refreshModels();
  }, [refreshRemote, refreshArchived, refreshModels]);

  /** 回填一页历史 turn → 只读 ItemView[]；turns/list 失败时回退 threadRead includeTurns */
  const fetchBackfill = useCallback(async (threadId: string): Promise<ItemView[]> => {
    try {
      const tl = await threadTurnsList({
        threadId,
        itemsView: "full",
        sortDirection: "asc",
        limit: 50,
      });
      return hydrateTurnsToItems(tl.data);
    } catch {
      try {
        const readRes = await threadRead({ threadId, includeTurns: true });
        return hydrateTurnsToItems(readRes.thread.turns);
      } catch {
        // 回填失败：简化为 resume 后不回填历史只续聊
        return [];
      }
    }
  }, []);

  const handleResume = useCallback(
    async (threadId: string) => {
      try {
        let resumed: ThreadResumeResult | null = null;
        try {
          // 复用 vaultRoot 关联的 model（若本地有记录）
          const preferredModel = vaultRoot
            ? (() => {
                try {
                  const rec = localActive.find((r) => r.threadId === threadId);
                  return rec?.model ?? model ?? null;
                } catch {
                  return model ?? null;
                }
              })()
            : model ?? null;
          // excludeTurns=true 仅取元数据（分页历史改用 turns/list），避免全量回放
          const resumeRes = await threadResume({
            threadId,
            excludeTurns: true,
            ...(preferredModel ? { model: preferredModel } : {}),
          });
          const thread = resumeRes.thread;
          const items = await fetchBackfill(threadId);
          // 会话显示名：远端 name（用户标题）优先，其次本地标题，最后预览；均无则回退 id 短码
          const localTitle = vaultRoot
            ? localActive.find((r) => r.threadId === threadId)?.title ?? null
            : null;
          const title = thread.name ?? localTitle ?? thread.preview ?? null;
          resumed = { items, title };
        } catch {
          // resume 失败：仍允许本地切 threadId（由 AgentPanel 接管 threadId 切换与 codec 重建）
          resumed = null;
        }
        onSelectThread(threadId, resumed);
      } catch (e: unknown) {
        setError(e instanceof Error ? e.message : String(e));
      }
    },
    [onSelectThread, vaultRoot, localActive, model, fetchBackfill],
  );

  const handleArchive = useCallback(
    async (threadId: string) => {
      const ok = window.confirm("归档该会话？归档后可在下方「已归档」区找回。");
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
      // 本地过滤会隐藏；同时从 remote 列表移除并刷新已归档区
      setRemoteThreads((prev) => prev.filter((t) => t.id !== threadId));
      void refreshArchived();
      onArchived(threadId);
    },
    [vaultRoot, onArchived, refreshArchived],
  );

  const handleUnarchive = useCallback(
    async (threadId: string) => {
      try {
        await threadUnarchive({ threadId });
      } catch {
        // 即便远端失败也清理本地 archived 标记
      }
      if (vaultRoot) {
        try {
          unarchiveThread(vaultRoot, threadId);
        } catch {
          // ignore
        }
      }
      setLocalVersion((n) => n + 1);
      setArchivedThreads((prev) => prev.filter((t) => t.id !== threadId));
      // 刷新未归档列表，使恢复的会话出现在活跃区
      void refreshRemote();
    },
    [vaultRoot, refreshRemote],
  );

  const handleDelete = useCallback(
    async (threadId: string, title: string) => {
      const label = title || threadId.slice(0, 8);
      const ok = window.confirm(`确定删除会话「${label}」？删除后不可恢复。`);
      if (!ok) return;
      try {
        await threadDelete({ threadId });
      } catch {
        // 远端失败也继续清理本地关联，避免幽灵条目
      }
      if (vaultRoot) {
        try {
          removeThread(vaultRoot, threadId);
        } catch {
          // ignore
        }
      }
      setLocalVersion((n) => n + 1);
      setRemoteThreads((prev) => prev.filter((t) => t.id !== threadId));
      setArchivedThreads((prev) => prev.filter((t) => t.id !== threadId));
      onDeleted(threadId);
    },
    [vaultRoot, onDeleted],
  );

  const localEntryTitle = (r: AgentThreadRecord): string => r.title || r.threadId.slice(0, 8);
  const remoteEntryTitle = (t: Thread, localTitle?: string): string =>
    localTitle || t.name || t.preview || t.id.slice(0, 8);

  return (
    <div className="flex h-full flex-col gap-3 p-3">
      {/* 顶部：模型选择 + 新建 */}
      <div className="flex items-center gap-2">
        <select
          value={model ?? ""}
          onChange={(e) => onModelChange(e.target.value || null)}
          className="h-8 min-w-0 flex-1 rounded-md border border-input bg-background px-2 text-xs"
          disabled={modelsLoading || (authMode !== "gateway" && !isRunning)}
        >
          <option value="">{modelsLoading ? "加载模型…" : models.length === 0 ? "无模型" : "选择模型"}</option>
          {models.map((m) => (
            <option key={m.id} value={m.id}>
              {m.label}
            </option>
          ))}
        </select>
        <Button type="button" size="sm" onClick={onNewThread} disabled={!isRunning}>
          新建会话
        </Button>
      </div>
      {modelsError && (
        <div className="flex items-center gap-2">
          <p className="flex-1 text-xs text-destructive">{modelsError}</p>
          {onOpenSettings && (
            <Button type="button" variant="outline" size="sm" className="h-6 shrink-0 px-2 text-xs" onClick={onOpenSettings}>
              打开设置
            </Button>
          )}
        </div>
      )}

      <div className="flex items-center justify-between">
        <p className="text-xs font-medium">会话列表</p>
        <Button type="button" variant="ghost" size="sm" className="h-7 text-xs" onClick={() => void refreshRemote()} disabled={loading || !isRunning}>
          刷新
        </Button>
      </div>

      {error && <p className="rounded bg-destructive/10 px-2 py-1 text-xs text-destructive">{error}</p>}
      {!isRunning && <p className="text-xs text-muted-foreground">Agent 未运行，启动后可查看会话。</p>}

      <ScrollArea className="min-h-0 flex-1">
        <div className="flex flex-col gap-3 pr-1">
        {/* 本地关联区 */}
        <div>
          <p className="mb-1 text-[11px] font-medium text-muted-foreground">本 vault 会话（{localActive.length}）</p>
          {localActive.length === 0 ? (
            <p className="text-xs text-muted-foreground">暂无本 vault 会话，发送消息将创建。</p>
          ) : (
            <ul className="space-y-1">
              {localActive.map((r) => (
                <li
                  key={r.threadId}
                  className={`group flex items-center gap-2 rounded border px-2 py-1.5 ${activeThreadId === r.threadId ? "border-primary bg-accent/40" : "bg-card"}`}
                >
                  <button
                    type="button"
                    onClick={() => void handleResume(r.threadId)}
                    className="min-w-0 flex-1 text-left"
                    title={r.threadId}
                  >
                    <p className="truncate text-xs font-medium">{localEntryTitle(r)}</p>
                    <p className="truncate text-[11px] text-muted-foreground">
                      {formatTime(r.createdAt)} {r.model ? `· ${r.model}` : ""}
                    </p>
                  </button>
                  <RowActions
                    primaryLabel="归档"
                    primaryTitle="归档该会话"
                    onPrimary={() => void handleArchive(r.threadId)}
                    onDelete={() => void handleDelete(r.threadId, r.title)}
                    deleteTitle="删除会话"
                  />
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
              const localTitle = localActive.find((r) => r.threadId === t.id)?.title;
              return (
                <li
                  key={t.id}
                  className={`group flex items-center gap-2 rounded border px-2 py-1.5 ${activeThreadId === t.id ? "border-primary bg-accent/40" : "bg-card"}`}
                >
                  <button
                    type="button"
                    onClick={() => void handleResume(t.id)}
                    className="min-w-0 flex-1 text-left"
                    title={t.id}
                  >
                    <p className="truncate text-xs">{remoteEntryTitle(t, localTitle)}</p>
                    <p className="truncate text-[11px] text-muted-foreground">
                      {formatTime(t.updatedAt)} {t.model ? `· ${t.model}` : ""} {t.status ? `· ${t.status}` : ""}
                    </p>
                  </button>
                  <RowActions
                    primaryLabel="归档"
                    primaryTitle="归档该会话"
                    onPrimary={() => void handleArchive(t.id)}
                    onDelete={() => void handleDelete(t.id, t.name || t.preview || t.id)}
                    deleteTitle="删除会话"
                  />
                </li>
              );
            })}
          </ul>
        </div>

        {/* 已归档折叠区 */}
        <div>
          <button
            type="button"
            onClick={() => setArchivedOpen((v) => !v)}
            className="mb-1 flex items-center gap-1.5 rounded px-1 text-[11px] font-medium text-muted-foreground hover:text-foreground"
            aria-expanded={archivedOpen}
          >
            <span className="font-mono">{archivedOpen ? "▾" : "▸"}</span>
            已归档（{filteredArchivedRemote.length}）
          </button>
          {archivedOpen && (
            filteredArchivedRemote.length === 0 ? (
              <p className="text-xs text-muted-foreground">暂无已归档会话。</p>
            ) : (
              <ul className="space-y-1">
                {filteredArchivedRemote.map((t) => {
                  const localTitle = localArchived.find((r) => r.threadId === t.id)?.title;
                  return (
                    <li
                      key={t.id}
                      className="group flex items-center gap-2 rounded border bg-card px-2 py-1.5 opacity-90"
                    >
                      <button
                        type="button"
                        onClick={() => void handleResume(t.id)}
                        className="min-w-0 flex-1 text-left"
                        title={t.id}
                      >
                        <p className="truncate text-xs">{remoteEntryTitle(t, localTitle)}</p>
                        <p className="truncate text-[11px] text-muted-foreground">
                          {formatTime(t.updatedAt)} {t.status ? `· ${t.status}` : ""}
                        </p>
                      </button>
                      <RowActions
                        primaryLabel="恢复"
                        primaryTitle="取消归档并恢复"
                        onPrimary={() => void handleUnarchive(t.id)}
                        onDelete={() => void handleDelete(t.id, t.name || t.preview || t.id)}
                        deleteTitle="删除已归档会话"
                      />
                    </li>
                  );
                })}
              </ul>
            )
          )}
        </div>
        </div>
      </ScrollArea>

      <p className="text-[11px] text-muted-foreground">
        回填策略：resume(excludeTurns) + threadTurnsList 分页一页（失败回退 threadRead includeTurns），仅摘要回放不完整。
      </p>
    </div>
  );
}
