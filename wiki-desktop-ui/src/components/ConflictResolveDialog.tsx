import { useEffect, useState, useCallback } from "react";
import { createPortal } from "react-dom";
import { Button } from "@/components/ui/button";
import { DiffView } from "@/components/DiffView";
import type { SyncState } from "@/lib/sync-engine";
import type { PendingConflict, Resolution } from "@/lib/conflict-resolve";
import { applyResolution } from "@/lib/conflict-resolve";
import { getNote, saveNote, writeSyncState } from "@/api/tauri";
import { createServerApi, loadSyncConfig } from "@/api/server";
import { CheckCircle2 } from "lucide-react";

type Props = {
  conflicts: PendingConflict[];
  state: SyncState;
  onResolved: (key: string) => void;
  onClose: () => void;
};

type Loaded = {
  local: { title: string; body: string };
  remote: { title: string; content: string; updated_at: string };
};

export function ConflictResolveDialog({ conflicts, state, onResolved, onClose }: Props) {
  const [selectedKey, setSelectedKey] = useState(() => conflicts[0]?.key ?? "");
  const [resolved, setResolved] = useState<Set<string>>(() => new Set());
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [resolving, setResolving] = useState(false);
  const [mergeMode, setMergeMode] = useState(false);
  const [mergeTitle, setMergeTitle] = useState("");
  const [mergeBody, setMergeBody] = useState("");

  const selected = conflicts.find((c) => c.key === selectedKey) ?? conflicts[0] ?? null;

  useEffect(() => {
    const h = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [onClose]);

  const load = useCallback(async () => {
    if (!selected) return;
    setLoading(true);
    setLoadError(null);
    setLoaded(null);
    setMergeMode(false);
    try {
      const cfg = loadSyncConfig();
      if (!cfg?.baseUrl || !cfg?.knowledgeId) throw new Error("未配置同步");
      const serverApi = createServerApi(cfg.baseUrl, cfg.knowledgeId);
      const [localDto, remotePage] = await Promise.all([
        getNote(selected.key),
        serverApi.getPage(selected.ref),
      ]);
      if (!remotePage) throw new Error("远端页面不存在");
      setLoaded({
        local: { title: localDto.title, body: localDto.body },
        remote: { title: remotePage.title, content: remotePage.content, updated_at: remotePage.updated_at },
      });
      setMergeTitle(localDto.title);
      setMergeBody(localDto.body);
    } catch (e: unknown) {
      setLoadError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [selected]);

  useEffect(() => {
    void load();
  }, [load]);

  const doResolve = async (res: Resolution) => {
    if (!selected || !loaded) return;
    setResolving(true);
    try {
      const cfg = loadSyncConfig();
      if (!cfg?.baseUrl || !cfg?.knowledgeId) throw new Error("未配置同步");
      const serverApi = createServerApi(cfg.baseUrl, cfg.knowledgeId);
      const io = {
        writeNote: (key: string, body: string, title?: string) => saveNote(key, body, title) as Promise<unknown>,
        putPage: (ref: string, page: { title: string; summary: string; content: string }) =>
          serverApi.putPage(ref, page) as Promise<{ updated_at: string; content: string }>,
        now: () => Math.floor(Date.now() / 1000),
      };
      await applyResolution(io, state, selected, loaded.local, loaded.remote, res);
      await writeSyncState(JSON.stringify(state));
      setResolved((prev) => new Set(prev).add(selected.key));
      onResolved(selected.key);
      setMergeMode(false);
    } catch (e: unknown) {
      window.alert(e instanceof Error ? e.message : String(e));
    } finally {
      setResolving(false);
    }
  };

  const allResolved = conflicts.length > 0 && conflicts.every((c) => resolved.has(c.key));

  const overlay = (
    <div
      data-modal-open=""
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="flex max-h-[85vh] w-[min(96vw,960px)] flex-col rounded-lg border border-border bg-popover shadow-xl"
        onMouseDown={(e) => e.stopPropagation()}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between border-b px-4 py-3">
          <h2 className="text-sm font-semibold">解决冲突</h2>
          <Button type="button" variant="ghost" size="sm" onClick={onClose}>
            关闭
          </Button>
        </div>

        {allResolved ? (
          <div className="flex flex-1 flex-col items-center justify-center gap-3 px-6 py-10">
            <CheckCircle2 className="size-8 text-green-500" />
            <p className="text-sm">全部解决</p>
            <Button type="button" onClick={onClose}>
              完成
            </Button>
          </div>
        ) : (
          <div className="flex min-h-0 flex-1">
            <div className="w-[200px] shrink-0 border-r overflow-auto">
              <ul className="p-2 space-y-1">
                {conflicts.map((c) => {
                  const isSelected = c.key === selectedKey;
                  const isResolved = resolved.has(c.key);
                  return (
                    <li key={c.key}>
                      <button
                        type="button"
                        onClick={() => setSelectedKey(c.key)}
                        className={`flex w-full items-center gap-1.5 rounded px-2 py-1.5 text-left text-xs ${isSelected ? "bg-accent text-accent-foreground" : "hover:bg-muted"}`}
                      >
                        <span className="min-w-0 flex-1 truncate font-mono">{c.key}</span>
                        {isResolved && <CheckCircle2 className="size-3.5 shrink-0 text-green-500" />}
                      </button>
                      <p className="px-2 text-[10px] text-muted-foreground truncate">
                        {new Date(c.localModified * 1000).toLocaleString()} / {c.remoteUpdatedAt}
                      </p>
                    </li>
                  );
                })}
              </ul>
            </div>

            <div className="flex min-w-0 flex-1 flex-col overflow-hidden">
              <div className="min-h-0 flex-1 overflow-auto p-4 space-y-3">
                {loading && <p className="text-sm text-muted-foreground">加载中…</p>}
                {loadError && <p className="text-sm text-destructive">{loadError}</p>}
                {loaded && !loading && !loadError && (
                  <>
                    {loaded.local.title !== loaded.remote.title && (
                      <div className="rounded border bg-amber-500/10 px-3 py-2 text-xs">
                        <p className="font-medium">标题不一致</p>
                        <p>本地：{loaded.local.title}</p>
                        <p>远端：{loaded.remote.title}</p>
                      </div>
                    )}
                    {mergeMode ? (
                      <div className="space-y-2">
                        <input
                          value={mergeTitle}
                          onChange={(e) => setMergeTitle(e.target.value)}
                          placeholder="标题"
                          className="w-full rounded border bg-background px-2 py-1 text-sm"
                        />
                        <textarea
                          value={mergeBody}
                          onChange={(e) => setMergeBody(e.target.value)}
                          rows={14}
                          className="w-full rounded border bg-background px-2 py-1 font-mono text-xs"
                        />
                      </div>
                    ) : (
                      <DiffView localText={loaded.local.body} remoteText={loaded.remote.content} />
                    )}
                  </>
                )}
              </div>

              {loaded && !loading && !loadError && (
                <div className="flex flex-wrap gap-2 border-t px-4 py-3">
                  {!mergeMode ? (
                    <>
                      <Button type="button" size="sm" disabled={resolving} onClick={() => void doResolve("local")}>
                        使用本地
                      </Button>
                      <Button type="button" size="sm" variant="secondary" disabled={resolving} onClick={() => void doResolve("remote")}>
                        使用远端
                      </Button>
                      <Button type="button" size="sm" variant="secondary" disabled={resolving} onClick={() => void doResolve("both")}>
                        保留两者
                      </Button>
                      <Button
                        type="button"
                        size="sm"
                        variant="outline"
                        disabled={resolving}
                        onClick={() => setMergeMode(true)}
                      >
                        合并编辑
                      </Button>
                    </>
                  ) : (
                    <>
                      <Button
                        type="button"
                        size="sm"
                        disabled={resolving}
                        onClick={() => void doResolve({ merged: { title: mergeTitle, body: mergeBody } })}
                      >
                        保存合并结果
                      </Button>
                      <Button type="button" size="sm" variant="ghost" disabled={resolving} onClick={() => setMergeMode(false)}>
                        取消
                      </Button>
                    </>
                  )}
                </div>
              )}
            </div>
          </div>
        )}
      </div>
    </div>
  );

  return createPortal(overlay, document.body);
}
