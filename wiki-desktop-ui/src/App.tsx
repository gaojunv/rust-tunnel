import { useCallback, useEffect, useRef, useState } from "react";
import { TitleBar } from "@/components/TitleBar";
import { WikiSidebar } from "@/components/WikiSidebar";
import { NoteEditor, type NoteEditorHandle } from "@/components/NoteEditor";
import { GraphPanel } from "@/components/GraphPanel";
import { RightPanel } from "@/components/RightPanel";
import { AiChatPanel } from "@/components/ai/AiChatPanel";
import { BacklinksPanel } from "@/components/BacklinksPanel";
import { TocPanel } from "@/components/TocPanel";
import { getNote, saveNote, vaultInfo } from "@/api/tauri";
import type { VaultInfo } from "@/api/types";
import { useNoteHistory } from "@/lib/use-note-history";
import { QuickSwitcher } from "@/components/QuickSwitcher";
import { SettingsDialog } from "@/components/SettingsDialog";
import { SyncReportDialog } from "@/components/SyncReportDialog";
import { ConflictResolveDialog } from "@/components/ConflictResolveDialog";
import { useSync } from "@/lib/use-sync";

export default function App() {
  const { current: selectedKey, canBack, canForward, navigate, back, forward, remove, replace, replacePrefix, removePrefix } =
    useNoteHistory();
  const [mode, setMode] = useState<"edit" | "preview">("preview");
  const [refreshToken, setRefreshToken] = useState(0);
  const [vault, setVault] = useState<VaultInfo | null>(null);
  const [editorDirty, setEditorDirty] = useState(false);
  const [switcherOpen, setSwitcherOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const editorRef = useRef<NoteEditorHandle>(null);
  const previewContainerRef = useRef<HTMLDivElement | null>(null);
  const bumpRefreshToken = useCallback(() => setRefreshToken((n) => n + 1), []);
  const {
    status: syncStatus,
    report,
    setReport,
    syncNow,
    scheduleAutoSync,
    pendingConflicts,
    syncState,
    clearPendingConflict,
  } = useSync({
    flushSave: () => editorRef.current?.flushSave() ?? Promise.resolve(),
    onRefresh: bumpRefreshToken,
    editorOpenKey: selectedKey,
    refreshToken,
    onNeedSettings: () => setSettingsOpen(true),
  });
  const syncing = syncStatus.phase === "syncing";
  const [reportDialogOpen, setReportDialogOpen] = useState(false);
  const [conflictOpen, setConflictOpen] = useState(false);

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
    async (nextKey: string) => {
      try {
        await editorRef.current?.flushSave();
      } catch {
        if (!window.confirm("保存失败，仍要离开吗？未保存的改动将丢失。")) return;
      }
      navigate(nextKey);
      setMode("preview");
    },
    [navigate],
  );

  const handleBack = useCallback(async () => {
    try {
      await editorRef.current?.flushSave();
    } catch {
      if (!window.confirm("保存失败，仍要离开吗？未保存的改动将丢失。")) return;
    }
    back();
    setMode("preview");
  }, [back]);

  const handleForward = useCallback(async () => {
    try {
      await editorRef.current?.flushSave();
    } catch {
      if (!window.confirm("保存失败，仍要离开吗？未保存的改动将丢失。")) return;
    }
    forward();
    setMode("preview");
  }, [forward]);

  const handleSaved = useCallback(() => {
    setRefreshToken((n) => n + 1);
    scheduleAutoSync();
  }, [scheduleAutoSync]);

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
      try {
        await editorRef.current?.flushSave();
      } catch {
        if (!window.confirm("保存失败，仍要离开吗？未保存的改动将丢失。")) return;
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
    [navigate],
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
        void handleBack();
      } else if (e.key === "ArrowRight") {
        e.preventDefault();
        void handleForward();
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


  const manualSyncPendingRef = useRef(false);
  const handleManualSync = useCallback(() => {
    manualSyncPendingRef.current = true;
    void syncNow();
  }, [syncNow]);
  useEffect(() => {
    if (!report) {
      setReportDialogOpen(false);
      return;
    }
    if (pendingConflicts.length > 0) {
      setConflictOpen(true);
      return;
    }
    if (manualSyncPendingRef.current) {
      manualSyncPendingRef.current = false;
      setReportDialogOpen(true);
      return;
    }
    if (report.errors > 0 || report.conflicts > 0) {
      setReportDialogOpen(true);
    }
  }, [report, pendingConflicts.length]);
  useEffect(() => {
    if (pendingConflicts.length > 0) setConflictOpen(true);
  }, [pendingConflicts.length]);
  return (
    <div className="flex h-screen flex-col bg-background">
      <TitleBar
        dirty={editorDirty}
        canBack={canBack}
        canForward={canForward}
        onBack={() => void handleBack()}
        onForward={() => void handleForward()}
        onOpenSwitcher={() => setSwitcherOpen(true)}
        onOpenSettings={() => setSettingsOpen(true)}
        syncing={syncing}
        syncStatus={syncStatus}
        onSyncNow={() => void handleManualSync()}
      />

      <div className="flex min-h-0 flex-1">
        <aside className="w-[300px] shrink-0 border-r bg-sidebar max-[900px]:w-[260px]">
          <WikiSidebar
            selectedKey={selectedKey}
            onSelect={(k) => void handleNavigate(k)}
            refreshToken={refreshToken}
            vaultInfo={vault}
            onCreateNote={(k, t) => void handleCreateNote(k, t)}
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
            onNavigate={(k) => void handleNavigate(k)}
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
                onNavigate={(k) => void handleNavigate(k)}
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
                onNavigate={(k) => void handleNavigate(k)}
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
        onSelect={(k) => void handleNavigate(k)}
      />
      {settingsOpen && (
        <SettingsDialog onClose={() => setSettingsOpen(false)} onSync={() => void handleManualSync()} />
      )}
      {conflictOpen && pendingConflicts.length > 0 && syncState && (
        <ConflictResolveDialog
          conflicts={pendingConflicts}
          state={syncState}
          onResolved={(k) => { clearPendingConflict(k); bumpRefreshToken(); }}
          onClose={() => { setConflictOpen(false); bumpRefreshToken(); }}
        />
      )}
      {reportDialogOpen && report && !(conflictOpen && pendingConflicts.length > 0) && (
        <SyncReportDialog report={report} onClose={() => { setReportDialogOpen(false); setReport(null); }} onNavigate={(k) => void handleNavigate(k)} />
      )}
    </div>
  );
}
