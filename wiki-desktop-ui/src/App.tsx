import { useCallback, useEffect, useRef, useState } from "react";
import { TitleBar } from "@/components/TitleBar";
import { WikiSidebar } from "@/components/WikiSidebar";
import { NoteEditor, type NoteEditorHandle } from "@/components/NoteEditor";
import { GraphPanel } from "@/components/GraphPanel";
import { RightPanel } from "@/components/RightPanel";
import { AiChatPanel } from "@/components/ai/AiChatPanel";
import { BacklinksPanel } from "@/components/BacklinksPanel";
import { TocPanel } from "@/components/TocPanel";
import { getNote, listNotesFull, readSyncState, saveNote, vaultInfo, writeSyncState } from "@/api/tauri";
import type { VaultInfo } from "@/api/types";
import { useNoteHistory } from "@/lib/use-note-history";
import { QuickSwitcher } from "@/components/QuickSwitcher";
import { SettingsDialog } from "@/components/SettingsDialog";
import { SyncReportDialog } from "@/components/SyncReportDialog";
import { createServerApi, loadSyncConfig } from "@/api/server";
import { AuthExpiredError, clearToken } from "@/lib/server-auth";
import { emptySyncState, hashNote, planSync, runSync, type LocalNote, type SyncReport, type SyncState } from "@/lib/sync-engine";

export default function App() {
  const { current: selectedKey, canBack, canForward, navigate, back, forward, remove, replace, replacePrefix, removePrefix } =
    useNoteHistory();
  const [mode, setMode] = useState<"edit" | "preview">("preview");
  const [refreshToken, setRefreshToken] = useState(0);
  const [vault, setVault] = useState<VaultInfo | null>(null);
  const [editorDirty, setEditorDirty] = useState(false);
  const [switcherOpen, setSwitcherOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [report, setReport] = useState<SyncReport | null>(null);
  const editorRef = useRef<NoteEditorHandle>(null);
  const previewContainerRef = useRef<HTMLDivElement | null>(null);

  const reloadVault = useCallback(() => {
    vaultInfo()
      .then(setVault)
      .catch(() => setVault(null));
  }, []);

  useEffect(() => {
    reloadVault();
  }, [reloadVault]);

  useEffect(() => {
    reloadVault();
  }, [refreshToken, reloadVault]);

  const handleNavigate = useCallback(
    (nextKey: string) => {
      if (editorDirty) {
        const ok = window.confirm("有未保存的改动，确定要切换笔记吗？未保存的内容会丢失。");
        if (!ok) return;
      }
      navigate(nextKey);
      setEditorDirty(false);
      setMode("preview");
    },
    [editorDirty, navigate],
  );

  const handleBack = useCallback(() => {
    if (editorDirty) {
      const ok = window.confirm("有未保存的改动，确定要切换笔记吗？未保存的内容会丢失。");
      if (!ok) return;
    }
    back();
    setEditorDirty(false);
    setMode("preview");
  }, [editorDirty, back]);

  const handleForward = useCallback(() => {
    if (editorDirty) {
      const ok = window.confirm("有未保存的改动，确定要切换笔记吗？未保存的内容会丢失。");
      if (!ok) return;
    }
    forward();
    setEditorDirty(false);
    setMode("preview");
  }, [editorDirty, forward]);

  const handleSaved = useCallback(() => {
    setRefreshToken((n) => n + 1);
  }, []);

  const handleDeleted = useCallback(
    (deletedKey?: string) => {
      const key = deletedKey ?? selectedKey;
      if (key) remove(key);
      setEditorDirty(false);
      setRefreshToken((n) => n + 1);
    },
    [selectedKey, remove],
  );

  const handleCreated = useCallback(
    async (key: string) => {
      try {
        await saveNote(key, "", key);
        setRefreshToken((n) => n + 1);
        navigate(key);
        setMode("edit");
      } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e);
        window.alert(msg);
      }
    },
    [navigate],
  );

  const handleCreateNote = useCallback(
    async (key: string, title: string) => {
      if (editorDirty) {
        const ok = window.confirm("有未保存的改动，确定要切换笔记吗？未保存的内容会丢失。");
        if (!ok) return;
      }
      try {
        await saveNote(key, "", title);
        setRefreshToken((n) => n + 1);
        navigate(key);
        setMode("edit");
      } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e);
        window.alert(msg);
      }
    },
    [editorDirty, navigate],
  );

  const handleRenamed = useCallback(
    (oldKey: string, newKey: string) => {
      replace(oldKey, newKey);
      setEditorDirty(false);
      setRefreshToken((n) => n + 1);
    },
    [replace],
  );

  const handleFolderChanged = useCallback(() => {
    setRefreshToken((n) => n + 1);
  }, []);

  const handleHistoryReplacePrefix = useCallback(
    (oldPrefix: string, newPrefix: string) => {
      replacePrefix(oldPrefix, newPrefix);
    },
    [replacePrefix],
  );

  const handleHistoryRemovePrefix = useCallback(
    (prefix: string) => {
      removePrefix(prefix);
    },
    [removePrefix],
  );

  // 同步主流程
  const handleSync = useCallback(async () => {
    if (editorDirty) {
      window.alert("有未保存的改动，请先保存后再同步。");
      return;
    }
    const cfg = loadSyncConfig();
    if (!cfg?.baseUrl || !cfg?.knowledgeId) {
      setSettingsOpen(true);
      return;
    }

    setSyncing(true);
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

      const serverApi = createServerApi(cfg.baseUrl, cfg.knowledgeId);
      const remote = await serverApi.listAllPages();

      const plan = planSync({ local, remote, state, propagateDeletes: cfg.propagateDeletes });
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
      setRefreshToken((n) => n + 1);
      setReport(syncReport);
    } catch (e: unknown) {
      if (e instanceof AuthExpiredError) {
        clearToken(cfg.baseUrl);
        window.alert("登录已过期，请重新登录");
        setSettingsOpen(true);
      } else {
        const msg = e instanceof Error ? e.message : String(e);
        window.alert(`同步失败：${msg}`);
      }
    } finally {
      setSyncing(false);
    }
  }, [editorDirty]);

  // 全局导航快捷键
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && !e.altKey) {
        const lower = e.key.toLowerCase();
        if (lower === "k" || lower === "p") {
          if (switcherOpen) {
            e.preventDefault();
            setSwitcherOpen(false);
            return;
          }
          if (document.querySelector("[data-modal-open]")) return;
          e.preventDefault();
          setSwitcherOpen(true);
          return;
        }
      }
      if (document.querySelector("[data-modal-open]")) return;
      const isAlt = e.altKey && !e.ctrlKey && !e.metaKey;
      const isCtrl = e.ctrlKey && !e.metaKey && !e.altKey;
      if (!isAlt && !isCtrl) return;
      if (e.key === "ArrowLeft") {
        e.preventDefault();
        handleBack();
      } else if (e.key === "ArrowRight") {
        e.preventDefault();
        handleForward();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [handleBack, handleForward, switcherOpen]);

  const handleAiInsert = useCallback((text: string) => {
    editorRef.current?.insertAtCursor(text);
  }, []);

  const getCurrentNoteForAi = useCallback(async () => {
    if (!selectedKey) return null;
    try {
      const n = await getNote(selectedKey);
      return n;
    } catch {
      return null;
    }
  }, [selectedKey]);

  const handleScrollToLine = useCallback((line: number) => {
    editorRef.current?.scrollToLine(line);
  }, []);

  return (
    <div className="flex h-screen flex-col bg-background">
      <TitleBar
        dirty={editorDirty}
        canBack={canBack}
        canForward={canForward}
        onBack={handleBack}
        onForward={handleForward}
        onOpenSwitcher={() => setSwitcherOpen(true)}
        onOpenSettings={() => setSettingsOpen(true)}
        syncing={syncing}
      />

      <div className="flex min-h-0 flex-1">
        <aside className="w-[300px] shrink-0 border-r bg-sidebar max-[900px]:w-[260px]">
          <WikiSidebar
            selectedKey={selectedKey}
            onSelect={handleNavigate}
            refreshToken={refreshToken}
            vaultInfo={vault}
            onCreateNote={handleCreateNote}
            onFolderChanged={handleFolderChanged}
            onHistoryReplacePrefix={handleHistoryReplacePrefix}
            onHistoryRemovePrefix={handleHistoryRemovePrefix}
          />
        </aside>

        <main className="min-w-0 flex-1 overflow-hidden bg-background">
          <NoteEditor
            key={selectedKey ?? "__none__"}
            ref={editorRef}
            noteKey={selectedKey}
            mode={mode}
            onModeChange={setMode}
            onSaved={handleSaved}
            onDeleted={handleDeleted}
            onDirtyChange={setEditorDirty}
            onNavigate={handleNavigate}
            onCreate={handleCreated}
            onRenamed={handleRenamed}
            onOpenSettings={() => setSettingsOpen(true)}
            refreshToken={refreshToken}
            previewContainerRef={previewContainerRef}
          />
        </main>

        <aside className="hidden w-[320px] shrink-0 border-l bg-sidebar xl:block">
          <RightPanel
            graphPanel={
              <GraphPanel
                selectedKey={selectedKey}
                refreshToken={refreshToken}
                onNavigate={handleNavigate}
                onCreate={handleCreated}
              />
            }
            aiPanel={
              <AiChatPanel
                onInsert={handleAiInsert}
                getCurrentNote={getCurrentNoteForAi}
                onOpenSettings={() => setSettingsOpen(true)}
              />
            }
            backlinksPanel={
              <BacklinksPanel
                selectedKey={selectedKey}
                refreshToken={refreshToken}
                onNavigate={handleNavigate}
              />
            }
            tocPanel={
              <TocPanel
                noteKey={selectedKey}
                getCurrentNote={getCurrentNoteForAi}
                refreshToken={refreshToken}
                mode={mode}
                onScrollToLine={handleScrollToLine}
                previewContainerRef={previewContainerRef as React.RefObject<HTMLElement | null>}
              />
            }
          />
        </aside>
      </div>
      <QuickSwitcher
        open={switcherOpen}
        onClose={() => setSwitcherOpen(false)}
        onSelect={handleNavigate}
      />
      {settingsOpen && (
        <SettingsDialog onClose={() => setSettingsOpen(false)} onSync={() => void handleSync()} />
      )}
      {report && (
        <SyncReportDialog report={report} onClose={() => setReport(null)} onNavigate={handleNavigate} />
      )}
    </div>
  );
}
