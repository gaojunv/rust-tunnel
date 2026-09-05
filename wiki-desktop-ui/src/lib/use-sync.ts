import { useCallback, useEffect, useRef, useState } from "react";
import { createServerApi, loadSyncConfig, ServerError } from "@/api/server";
import { AuthExpiredError, clearToken, getToken } from "@/lib/server-auth";
import {
  emptySyncState,
  hashNote,
  planSync,
  runSync,
  type LocalNote,
  type SyncReport,
  type SyncState,
} from "@/lib/sync-engine";
import { ensureCompatibleRefs } from "@/lib/compat-refs";
import { computePendingCount } from "@/lib/pending";
import { conflictsFromReport, type PendingConflict } from "@/lib/conflict-resolve";
import { listNotesFull, readSyncState, saveNote, setNoteRef, writeSyncState } from "@/api/tauri";

export type SyncPhase = "disabled" | "idle" | "syncing" | "offline" | "error";
export type SyncStatus = {
  phase: SyncPhase;
  pendingCount: number | null;
  lastSyncAt: number | null;
  lastError: string | null;
};

function lastAtKey(knowledgeId: string): string {
  return `wiki.sync.lastAt.${knowledgeId}`;
}

function readLastAt(knowledgeId: string): number | null {
  try {
    const v = localStorage.getItem(lastAtKey(knowledgeId));
    if (!v) return null;
    const n = Number(v);
    return Number.isFinite(n) ? n : null;
  } catch {
    return null;
  }
}

function writeLastAt(knowledgeId: string, at: number): void {
  try {
    localStorage.setItem(lastAtKey(knowledgeId), String(at));
  } catch {
    // ignore
  }
}

function isOfflineError(e: unknown): boolean {
  if (e instanceof ServerError && e.status === 0) return true;
  if (e instanceof TypeError) {
    const msg = (e.message || "").toLowerCase();
    if (msg.includes("fetch") || msg.includes("network") || msg.includes("failed")) return true;
    return true;
  }
  if (e instanceof Error) {
    const msg = (e.message || "").toLowerCase();
    if (msg.includes("failed to fetch") || msg.includes("networkerror") || msg.includes("load failed"))
      return true;
  }
  return false;
}

const BACKOFF_STEPS = [30_000, 120_000, 600_000];

export function useSync(opts: {
  flushSave: () => Promise<void>;
  onRefresh: () => void;
  editorOpenKey: string | null;
  refreshToken: number;
  onNeedSettings?: () => void;
}): {
  status: SyncStatus;
  report: SyncReport | null;
  setReport(r: SyncReport | null): void;
  syncNow(): Promise<void>;
  scheduleAutoSync(): void;
  pendingConflicts: PendingConflict[];
  syncState: SyncState | null;
  clearPendingConflict(key: string): void;
} {
  const { flushSave, onRefresh, refreshToken, onNeedSettings } = opts;
  // editorOpenKey kept for spec compatibility (unused but listed in opts)
  void opts.editorOpenKey;

  const flushSaveRef = useRef(flushSave);
  useEffect(() => {
    flushSaveRef.current = flushSave;
  }, [flushSave]);
  const onRefreshRef = useRef(onRefresh);
  useEffect(() => {
    onRefreshRef.current = onRefresh;
  }, [onRefresh]);
  const onNeedSettingsRef = useRef(onNeedSettings);
  useEffect(() => {
    onNeedSettingsRef.current = onNeedSettings;
  }, [onNeedSettings]);

  const [report, setReport] = useState<SyncReport | null>(null);
  const [pendingConflicts, setPendingConflicts] = useState<PendingConflict[]>([]);
  const [syncState, setSyncState] = useState<SyncState | null>(null);

  const initialCfg = loadSyncConfig();
  const initialLastAt = initialCfg?.knowledgeId ? readLastAt(initialCfg.knowledgeId) : null;
  const initialToken = initialCfg?.baseUrl ? getToken(initialCfg.baseUrl) : null;
  const initialDisabled = !initialCfg?.baseUrl || !initialCfg?.knowledgeId || !initialToken;

  const [status, setStatus] = useState<SyncStatus>({
    phase: initialDisabled ? "disabled" : "idle",
    pendingCount: null,
    lastSyncAt: initialLastAt,
    lastError: null,
  });
  const statusRef = useRef(status);
  useEffect(() => {
    statusRef.current = status;
  }, [status]);

  const syncingRef = useRef(false);
  const pendingTimerRef = useRef<number | null>(null);
  const autoSyncTimerRef = useRef<number | null>(null);
  const intervalTimerRef = useRef<number | null>(null);
  const offlineTimerRef = useRef<number | null>(null);
  const offlineStepRef = useRef(0);
  const doSyncRef = useRef<(silent: boolean) => Promise<void>>(async () => {});

  const clearOfflineTimer = useCallback(() => {
    if (offlineTimerRef.current !== null) {
      window.clearTimeout(offlineTimerRef.current);
      offlineTimerRef.current = null;
    }
  }, []);

  const recomputePending = useCallback(async () => {
    const cfg = loadSyncConfig();
    if (!cfg?.baseUrl || !cfg?.knowledgeId) {
      setStatus((s) => ({ ...s, phase: "disabled", pendingCount: null }));
      return;
    }
    const token = getToken(cfg.baseUrl);
    if (!token) {
      setStatus((s) => ({ ...s, phase: "disabled", pendingCount: null }));
      return;
    }
    try {
      const notes = await listNotesFull();
      const raw = await readSyncState().catch(() => null);
      let state: SyncState | null = null;
      if (raw) {
        try {
          const parsed = JSON.parse(raw) as SyncState;
          if (parsed && parsed.knowledgeId === cfg.knowledgeId && parsed.version === 1) state = parsed;
        } catch {
          // ignore
        }
      }
      const cnt = await computePendingCount(
        notes.map((dto) => ({
          key: dto.key,
          title: dto.title,
          body: dto.body,
          refId: (dto.ref_id as string | null | undefined) ?? null,
        })),
        state,
      );
      setStatus((s) => {
        if (s.phase === "syncing" || s.phase === "offline") return { ...s, pendingCount: cnt };
        return { ...s, pendingCount: cnt };
      });
    } catch {
      // ignore
    }
  }, []);

  const schedulePendingRecompute = useCallback(() => {
    if (pendingTimerRef.current !== null) window.clearTimeout(pendingTimerRef.current);
    pendingTimerRef.current = window.setTimeout(() => {
      pendingTimerRef.current = null;
      void recomputePending();
    }, 400);
  }, [recomputePending]);

  useEffect(() => {
    schedulePendingRecompute();
  }, [refreshToken, schedulePendingRecompute]);

  useEffect(() => {
    void recomputePending();
  }, [recomputePending]);

  const resetIntervalTimer = useCallback(() => {
    if (intervalTimerRef.current !== null) {
      window.clearInterval(intervalTimerRef.current);
      intervalTimerRef.current = null;
    }
    const cfg = loadSyncConfig();
    const mins = cfg?.syncIntervalMinutes ?? 0;
    if (!mins || mins <= 0) return;
    const token = cfg?.baseUrl ? getToken(cfg.baseUrl) : null;
    if (!cfg?.baseUrl || !cfg?.knowledgeId || !token) return;
    const ms = mins * 60_000;
    intervalTimerRef.current = window.setInterval(() => {
      void doSyncRef.current(true);
    }, ms);
  }, []);

  useEffect(() => {
    const handler = () => {
      const cfg = loadSyncConfig();
      const token = cfg?.baseUrl ? getToken(cfg.baseUrl) : null;
      const disabled = !cfg?.baseUrl || !cfg?.knowledgeId || !token;
      setStatus((s) => ({
        ...s,
        phase: disabled ? "disabled" : s.phase === "disabled" ? "idle" : s.phase,
        lastSyncAt: cfg?.knowledgeId ? readLastAt(cfg.knowledgeId) : s.lastSyncAt,
        lastError: disabled ? null : s.lastError,
      }));
      if (cfg?.knowledgeId) {
        const last = readLastAt(cfg.knowledgeId);
        setStatus((s) => ({ ...s, lastSyncAt: last }));
      }
      schedulePendingRecompute();
      resetIntervalTimer();
    };
    window.addEventListener("wiki:syncConfigChanged", handler as EventListener);
    window.addEventListener("storage", handler as EventListener);
    return () => {
      window.removeEventListener("wiki:syncConfigChanged", handler as EventListener);
      window.removeEventListener("storage", handler as EventListener);
    };
  }, [resetIntervalTimer, schedulePendingRecompute]);

  useEffect(() => {
    const onOnline = () => {
      if (statusRef.current.phase === "offline") {
        clearOfflineTimer();
        offlineStepRef.current = 0;
        void doSyncRef.current(true);
      }
    };
    window.addEventListener("online", onOnline);
    return () => window.removeEventListener("online", onOnline);
  }, [clearOfflineTimer]);

  useEffect(() => {
    resetIntervalTimer();
    return () => {
      if (intervalTimerRef.current !== null) window.clearInterval(intervalTimerRef.current);
    };
  }, [resetIntervalTimer]);

  const doSyncInner = useCallback(
    async (silent: boolean) => {
      if (syncingRef.current) return;
      try {
        await flushSaveRef.current();
      } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e);
        if (!window.confirm(`保存失败：${msg}\n仍要继续同步吗？未保存的改动可能丢失。`)) return;
      }

      const cfg = loadSyncConfig();
      if (!cfg?.baseUrl || !cfg?.knowledgeId) {
        onNeedSettingsRef.current?.();
        return;
      }

      if (typeof navigator !== "undefined" && navigator.onLine === false) {
        setStatus((s) => ({ ...s, phase: "offline", lastError: "离线" }));
        // schedule backoff
        if (offlineTimerRef.current !== null) window.clearTimeout(offlineTimerRef.current);
        const delay = BACKOFF_STEPS[Math.min(offlineStepRef.current, BACKOFF_STEPS.length - 1)];
        offlineStepRef.current += 1;
        offlineTimerRef.current = window.setTimeout(() => {
          offlineTimerRef.current = null;
          void doSyncRef.current(true);
        }, delay);
        return;
      }

      const token = getToken(cfg.baseUrl);
      if (!token) {
        setStatus((s) => ({ ...s, phase: "disabled" }));
        onNeedSettingsRef.current?.();
        return;
      }

      syncingRef.current = true;
      setStatus((s) => ({ ...s, phase: "syncing", lastError: null }));

      const scheduleOfflineRetry = () => {
        if (offlineTimerRef.current !== null) window.clearTimeout(offlineTimerRef.current);
        const delay = BACKOFF_STEPS[Math.min(offlineStepRef.current, BACKOFF_STEPS.length - 1)];
        offlineStepRef.current += 1;
        offlineTimerRef.current = window.setTimeout(() => {
          offlineTimerRef.current = null;
          void doSyncRef.current(true);
        }, delay);
      };

      try {
        const notes = await listNotesFull();
        const local: LocalNote[] = [];
        const localByKey = new Map<string, LocalNote>();
        for (const dto of notes) {
          const h = await hashNote(dto.title, dto.body);
          const ln: LocalNote = {
            key: dto.key,
            refId: (dto.ref_id as string | null | undefined) ?? null,
            title: dto.title,
            body: dto.body,
            modified: dto.modified,
            contentHash: h,
          };
          local.push(ln);
          localByKey.set(dto.key, ln);
        }

        let state: SyncState;
        try {
          const raw = await readSyncState();
          if (!raw) {
            state = emptySyncState(cfg.knowledgeId);
          } else {
            const parsed = JSON.parse(raw) as SyncState;
            if (!parsed || parsed.knowledgeId !== cfg.knowledgeId || parsed.version !== 1) {
              state = emptySyncState(cfg.knowledgeId);
            } else {
              state = parsed;
            }
          }
        } catch {
          state = emptySyncState(cfg.knowledgeId);
        }

        await ensureCompatibleRefs(local, async (key, ref) => {
          const dto = await setNoteRef(key, ref);
          return { modified: dto.modified };
        });

        const serverApi = createServerApi(cfg.baseUrl, cfg.knowledgeId);
        const remote = await serverApi.listAllPages();

        const plan = planSync({ local, remote, state, propagateDeletes: cfg.propagateDeletes, deferConflicts: true });
        const io = {
          local: {
            writeNote: async (key: string, title: string, body: string) => {
              const dto = await saveNote(key, body, title);
              return { modified: dto.modified };
            },
          },
          remote: serverApi,
          now: () => Math.floor(Date.now() / 1000),
        };
        const syncReport = await runSync(plan, { localByKey, io, state });

        await writeSyncState(JSON.stringify(state));
        setSyncState({ ...state, entries: { ...state.entries } });
        setPendingConflicts(conflictsFromReport(syncReport));
        const at = Date.now();
        writeLastAt(cfg.knowledgeId, at);
        setStatus((s) => ({
          ...s,
          phase: "idle",
          lastSyncAt: at,
          lastError: syncReport.errors > 0 ? `${syncReport.errors} 个错误` : null,
        }));
        if (syncReport.errors > 0) {
          setStatus((s) => ({ ...s, phase: "error", lastError: `${syncReport.errors} 个错误` }));
        } else if (syncReport.conflicts > 0) {
          setStatus((s) => ({ ...s, phase: "idle", lastError: `${syncReport.conflicts} 个冲突` }));
        } else {
          clearOfflineTimer();
          offlineStepRef.current = 0;
        }

        // Auto-sync silent unless errors/conflicts: only open dialog when needed
        if (!silent || syncReport.errors > 0 || syncReport.conflicts > 0) {
          setReport(syncReport);
        } else {
          // Still update report state silently but mark to suppress dialog
          // We set report but App will check silent marker; simplest: still set but let App decide
          setReport(syncReport);
        }

        onRefreshRef.current();
        void recomputePending();
      } catch (e: unknown) {
        if (e instanceof AuthExpiredError) {
          const cfg2 = loadSyncConfig();
          if (cfg2?.baseUrl) clearToken(cfg2.baseUrl);
          window.alert("登录已过期，请重新登录");
          setStatus((s) => ({ ...s, phase: "disabled", lastError: "登录已过期" }));
          onNeedSettingsRef.current?.();
        } else if (isOfflineError(e)) {
          const msg = e instanceof Error ? e.message : String(e);
          setStatus((s) => ({ ...s, phase: "offline", lastError: msg || "离线" }));
          scheduleOfflineRetry();
          if (!silent) window.alert(`同步失败：${e instanceof Error ? e.message : String(e)}`);
        } else {
          const msg = e instanceof Error ? e.message : String(e);
          setStatus((s) => ({ ...s, phase: "error", lastError: msg }));
          if (!silent) window.alert(`同步失败：${msg}`);
          else
            setReport({
              items: [],
              uploaded: 0,
              downloaded: 0,
              conflicts: 0,
              restored: 0,
              deletedRemote: 0,
              skipped: 0,
              errors: 1,
            });
        }
      } finally {
        syncingRef.current = false;
        setStatus((s) => (s.phase === "syncing" ? { ...s, phase: "idle" } : s));
        void recomputePending();
      }
    },
    [clearOfflineTimer, recomputePending],
  );

  useEffect(() => {
    doSyncRef.current = doSyncInner;
  }, [doSyncInner]);

  const syncNow = useCallback(async () => {
    await doSyncRef.current(false);
  }, []);

  const scheduleAutoSync = useCallback(() => {
    const cfg = loadSyncConfig();
    if (cfg?.autoSyncAfterSave === false) return;
    if (autoSyncTimerRef.current !== null) window.clearTimeout(autoSyncTimerRef.current);
    autoSyncTimerRef.current = window.setTimeout(() => {
      autoSyncTimerRef.current = null;
      void doSyncRef.current(true);
    }, 30_000);
  }, []);

  useEffect(() => {
    return () => {
      if (pendingTimerRef.current !== null) window.clearTimeout(pendingTimerRef.current);
      if (autoSyncTimerRef.current !== null) window.clearTimeout(autoSyncTimerRef.current);
      if (intervalTimerRef.current !== null) window.clearInterval(intervalTimerRef.current);
      if (offlineTimerRef.current !== null) window.clearTimeout(offlineTimerRef.current);
    };
  }, []);

  const clearPendingConflict = (key: string) => {
    setPendingConflicts((prev) => prev.filter((c) => c.key !== key));
  };
  return { status, report, setReport, syncNow, scheduleAutoSync, pendingConflicts, syncState, clearPendingConflict };
}
