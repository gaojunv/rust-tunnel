import { useCallback, useEffect, useRef, useState } from "react";
import { TitleBar } from "@/components/TitleBar";
import { WikiSidebar } from "@/components/WikiSidebar";
import { NoteEditor, type NoteEditorHandle } from "@/components/NoteEditor";
import { GraphPanel } from "@/components/GraphPanel";
import { RightPanel } from "@/components/RightPanel";
import { AiChatPanel } from "@/components/ai/AiChatPanel";
import { BacklinksPanel } from "@/components/BacklinksPanel";
import { TocPanel } from "@/components/TocPanel";
import { PanelResizer } from "@/components/PanelResizer";
import { getNote, saveNote, vaultInfo } from "@/api/tauri";
import type { VaultInfo } from "@/api/types";
import { useNoteHistory } from "@/lib/use-note-history";
import { QuickSwitcher } from "@/components/QuickSwitcher";
import { SettingsDialog } from "@/components/SettingsDialog";
import { SyncReportDialog } from "@/components/SyncReportDialog";
import { ConflictResolveDialog } from "@/components/ConflictResolveDialog";
import { useSync } from "@/lib/use-sync";
import {
  PANEL_LIMITS,
  defaultRightCollapsed,
  loadPanelLayout,
  resolveDragWidth,
  savePanelLayout,
  type PanelLayout,
} from "@/lib/panel-layout";

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

  // 侧栏布局：宽度/折叠 + 拖拽态（拖拽中禁用 width 过渡）
  const [layout, setLayout] = useState<PanelLayout>(() => {
    const loaded = loadPanelLayout();
    try {
      const raw = window.localStorage.getItem("wiki.layout.v1");
      if (raw == null) return loaded;
      const parsed = JSON.parse(raw) as Record<string, unknown>;
      if (typeof parsed.rightCollapsed !== "boolean") {
        return { ...loaded, rightCollapsed: defaultRightCollapsed(window.innerWidth) };
      }
      return loaded;
    } catch {
      return loaded;
    }
  });
  const [draggingSide, setDraggingSide] = useState<"left" | "right" | null>(null);

  useEffect(() => {
    savePanelLayout(layout);
  }, [layout]);

  const toggleLeft = useCallback(() => {
    setLayout((prev) => ({ ...prev, leftCollapsed: !prev.leftCollapsed }));
  }, []);
  const toggleRight = useCallback(() => {
    setLayout((prev) => ({ ...prev, rightCollapsed: !prev.rightCollapsed }));
  }, []);

  const handleDragLeft = useCallback((rawWidth: number) => {
    const r = resolveDragWidth("left", rawWidth);
    if (r.collapsed) {
      setLayout((prev) => ({ ...prev, leftCollapsed: true }));
      return;
    }
    setLayout((prev) => ({ ...prev, leftCollapsed: false, leftWidth: r.width }));
  }, []);

  const handleDragRight = useCallback((rawWidth: number) => {
    const r = resolveDragWidth("right", rawWidth);
    if (r.collapsed) {
      setLayout((prev) => ({ ...prev, rightCollapsed: true }));
      return;
    }
    setLayout((prev) => ({ ...prev, rightCollapsed: false, rightWidth: r.width }));
  }, []);

  const handleResetLeft = useCallback(() => {
    setLayout((prev) => ({ ...prev, leftCollapsed: false, leftWidth: PANEL_LIMITS.left.defaultWidth }));
  }, []);
  const handleResetRight = useCallback(() => {
    setLayout((prev) => ({ ...prev, rightCollapsed: false, rightWidth: PANEL_LIMITS.right.defaultWidth }));
  }, []);

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

  // 全局快捷键：导航 + 侧栏折叠
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      // 侧栏折叠快捷键（在输入框内不触发）
      const target = e.target as HTMLElement | null;
      const inInput =
        target != null && (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable);
      if (!inInput) {
        const isMod = e.ctrlKey || e.metaKey;
        const lower = e.key.toLowerCase();
        if (isMod && lower === "b") {
          if (e.altKey) {
            if (!document.querySelector("[data-modal-open]")) {
              e.preventDefault();
              toggleRight();
              return;
            }
          } else {
            if (!document.querySelector("[data-modal-open]")) {
              e.preventDefault();
              toggleLeft();
              return;
            }
          }
        }
      }

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
  }, [handleBack, handleForward, switcherOpen, toggleLeft, toggleRight]);

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
        leftVisible={!layout.leftCollapsed}
        rightVisible={!layout.rightCollapsed}
        onToggleLeft={toggleLeft}
        onToggleRight={toggleRight}
      />

      <div className="flex min-h-0 flex-1">
        <aside
          className={`relative shrink-0 overflow-hidden bg-sidebar ${layout.leftCollapsed ? "" : "border-r"} ${draggingSide ? "" : "transition-[width] duration-200"}`}
          style={{ width: layout.leftCollapsed ? 0 : layout.leftWidth }}
          aria-hidden={layout.leftCollapsed}
        >
          <div
            className="h-full w-full overflow-hidden"
            style={{
              width: layout.leftCollapsed ? 0 : layout.leftWidth,
              minWidth: layout.leftCollapsed ? 0 : layout.leftWidth,
            }}
          >
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
          </div>
          {!layout.leftCollapsed && (
            <PanelResizer
              side="left"
              currentWidth={layout.leftWidth}
              dragging={draggingSide === "left"}
              onDrag={(w) => {
                if (draggingSide !== "left") setDraggingSide("left");
                handleDragLeft(w);
              }}
              onDragEnd={() => setDraggingSide(null)}
              onReset={handleResetLeft}
            />
          )}
          <span className="sr-only" aria-hidden>
            左侧栏
          </span>
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

        <aside
          className={`relative hidden shrink-0 overflow-hidden bg-sidebar xl:flex xl:flex-col ${layout.rightCollapsed ? "" : "border-l"} ${draggingSide ? "" : "transition-[width] duration-200"}`}
          style={{ width: layout.rightCollapsed ? 0 : layout.rightWidth }}
          aria-hidden={layout.rightCollapsed}
        >
          <div
            className="flex h-full w-full flex-col overflow-hidden"
            style={{
              width: layout.rightCollapsed ? 0 : layout.rightWidth,
              minWidth: layout.rightCollapsed ? 0 : layout.rightWidth,
            }}
          >
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
          </div>
          {!layout.rightCollapsed && (
            <PanelResizer
              side="right"
              currentWidth={layout.rightWidth}
              dragging={draggingSide === "right"}
              onDrag={(w) => {
                if (draggingSide !== "right") setDraggingSide("right");
                handleDragRight(w);
              }}
              onDragEnd={() => setDraggingSide(null)}
              onReset={handleResetRight}
            />
          )}
        </aside>
      </div>
      <QuickSwitcher
        open={switcherOpen}
        onClose={() => setSwitcherOpen(false)}
        onSelect={(k) => void handleNavigate(k)}
      />
      {settingsOpen && <SettingsDialog onClose={() => setSettingsOpen(false)} onSync={() => void handleManualSync()} />}
      {conflictOpen && pendingConflicts.length > 0 && syncState && (
        <ConflictResolveDialog
          conflicts={pendingConflicts}
          state={syncState}
          onResolved={(k) => {
            clearPendingConflict(k);
            bumpRefreshToken();
          }}
          onClose={() => {
            setConflictOpen(false);
            bumpRefreshToken();
          }}
        />
      )}
      {reportDialogOpen && report && !(conflictOpen && pendingConflicts.length > 0) && (
        <SyncReportDialog
          report={report}
          onClose={() => {
            setReportDialogOpen(false);
            setReport(null);
          }}
          onNavigate={(k) => void handleNavigate(k)}
        />
      )}
    </div>
  );
}
