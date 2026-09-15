import { lazy, memo, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { TitleBar } from "@/components/TitleBar";
import { WikiSidebar } from "@/components/WikiSidebar";
import { NoteEditor, type NoteEditorHandle } from "@/components/NoteEditor";
import { GraphPanel } from "@/components/GraphPanel";
import { RightPanel } from "@/components/RightPanel";
// AgentPanel 重量级（含 streamdown/diff 链）：React.lazy 拆独立 chunk，
// 首屏不加载；RightPanel 仅 agent tab 切入才挂载（keepMounted 语义不变）。
const AgentPanel = lazy(() =>
  import("@/components/agent/AgentPanel").then((m) => ({ default: m.AgentPanel })),
);
import { BacklinksPanel } from "@/components/BacklinksPanel";
import { TocPanel } from "@/components/TocPanel";
import { PanelResizer } from "@/components/PanelResizer";
import { getNote, saveNote, vaultInfo } from "@/api/tauri";
import type { VaultInfo } from "@/api/types";
import { useNoteHistory } from "@/lib/use-note-history";
import { QuickSwitcher } from "@/components/QuickSwitcher";
import { SettingsDialog } from "@/components/SettingsDialog";
import { SyncReportDialog } from "@/components/SyncReportDialog";
// ConflictResolveDialog 含 DiffView→diff 链：lazy 后 diff 首屏不再被主包引用，
// 与 streamdown 同进 vendor-markdown 按需 chunk（冲突弹窗打开时才加载）。
const ConflictResolveDialog = lazy(() =>
  import("@/components/ConflictResolveDialog").then((m) => ({
    default: m.ConflictResolveDialog,
  })),
);
import { ToastHost } from "@/components/ToastHost";
import { toast } from "@/lib/toast";
import { useSync } from "@/lib/use-sync";
import {
  PANEL_LIMITS,
  defaultRightCollapsed,
  loadPanelLayout,
  resolveDragWidth,
  savePanelLayout,
  type PanelLayout,
} from "@/lib/panel-layout";

/**
 * 解析 wikilink 导航目标 `[[note#heading]]`：
 * - 返回 `[noteKey, heading?]`，heading 为 `#` 之后的部分（可含多级 `#`）；
 * - `[[#heading]]`（纯锚点）返回 `["", heading]`，调用方做同笔记内滚动；
 * - 其他调用方（侧栏/搜索/图谱/报告）传的是纯 key，不含 `#`，原样返回。
 */
export function parseWikilinkTarget(target: string): [string, string | null] {
  const hashIdx = target.indexOf("#");
  if (hashIdx < 0) return [target, null];
  const notePart = target.slice(0, hashIdx);
  const heading = target.slice(hashIdx + 1).trim();
  return [notePart, heading ? heading : null];
}

/**
 * 可变宽侧栏容器（memo）：拖拽中仅当 width/collapsed/drill-in props 变化时重渲染，
 * App 整树其他 state（编辑器 body、agent 流等）变化时跳过。
 * 拖拽中宽度经 dragWidth prop 传入（局部态），不经过 layout。
 */
const ResizableAside = memo(function ResizableAside({
  side,
  width,
  collapsed,
  dragging,
  extraClassName,
  contentClassName,
  children,
  resizer,
  label,
}: {
  side: "left" | "right";
  width: number;
  collapsed: boolean;
  dragging: boolean;
  extraClassName: string;
  contentClassName: string;
  children: React.ReactNode;
  resizer: React.ReactNode;
  label: string;
}) {
  const w = collapsed ? 0 : width;
  return (
    <aside
      className={`relative shrink-0 overflow-hidden bg-sidebar ${collapsed ? "" : side === "left" ? "border-r" : "border-l"} ${dragging ? "" : "transition-[width] duration-200"} ${extraClassName}`}
      style={{ width: w }}
      aria-hidden={collapsed}
    >
      <div className={contentClassName} style={{ width: w, minWidth: w }}>
        {children}
      </div>
      {!collapsed && resizer}
      <span className="sr-only" aria-hidden>
        {label}
      </span>
    </aside>
  );
});

export default function App() {
  const { current: selectedKey, canBack, canForward, navigate, back, forward, remove, replace, replacePrefix, removePrefix } =
    useNoteHistory();
  // 跨笔记锚点跳转的待滚动标题：navigate 到目标笔记后，NoteEditor 加载完成时消费
  const [pendingAnchor, setPendingAnchor] = useState<{ key: string; heading: string } | null>(null);
  const [refreshToken, setRefreshToken] = useState(0);
  const [vault, setVault] = useState<VaultInfo | null>(null);
  const [editorDirty, setEditorDirty] = useState(false);
  const [switcherOpen, setSwitcherOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const editorRef = useRef<NoteEditorHandle>(null);
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
    refreshToken,
    onNeedSettings: () => setSettingsOpen(true),
  });
  const syncing = syncStatus.phase === "syncing";
  const [reportDialogOpen, setReportDialogOpen] = useState(false);
  const [conflictOpen, setConflictOpen] = useState(false);

  // 侧栏布局：宽度/折叠 + 拖拽态（拖拽中禁用 width 过渡）
  // 拖拽中宽度下沉为 dragWidthRef + dragWidthTick 局部态：pointermove 只触发
  // ResizableAside 重渲染（React.memo + 局部宽度 prop），整树不受影响；
  // 拖拽中不写 layout（不触发 savePanelLayout effect），onDragEnd 一次提交。
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
  // 拖拽中的临时宽度（null = 非拖拽，用 layout 值）
  const [dragWidth, setDragWidth] = useState<{ side: "left" | "right"; width: number } | null>(null);

  useEffect(() => {
    savePanelLayout(layout);
  }, [layout]);

  const toggleLeft = useCallback(() => {
    setLayout((prev) => ({ ...prev, leftCollapsed: !prev.leftCollapsed }));
  }, []);
  const toggleRight = useCallback(() => {
    setLayout((prev) => ({ ...prev, rightCollapsed: !prev.rightCollapsed }));
  }, []);

  // 拖拽中：仅更新 dragWidth 局部态（稳定的 useCallback，PanelResizer 的 ref 透传下也无妨）
  const handleDragPreview = useCallback((side: "left" | "right", rawWidth: number) => {
    setDragWidth({ side, width: rawWidth });
  }, []);

  // 拖拽结束：resolve + 一次性提交 layout（单次 setItem 在上面的 effect 里）
  const handleDragEndLeft = useCallback(() => {
    setDragWidth((dw) => {
      if (dw && dw.side === "left") {
        const r = resolveDragWidth("left", dw.width);
        if (r.collapsed) setLayout((prev) => ({ ...prev, leftCollapsed: true }));
        else setLayout((prev) => ({ ...prev, leftCollapsed: false, leftWidth: r.width }));
      }
      return null;
    });
    setDraggingSide(null);
  }, []);

  const handleDragEndRight = useCallback(() => {
    setDragWidth((dw) => {
      if (dw && dw.side === "right") {
        const r = resolveDragWidth("right", dw.width);
        if (r.collapsed) setLayout((prev) => ({ ...prev, rightCollapsed: true }));
        else setLayout((prev) => ({ ...prev, rightCollapsed: false, rightWidth: r.width }));
      }
      return null;
    });
    setDraggingSide(null);
  }, []);

  const handleDragStartLeft = useCallback(() => setDraggingSide("left"), []);
  const handleDragStartRight = useCallback(() => setDraggingSide("right"), []);

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

  // wikilink 锚点跳转 `[[note#heading]]` / `[[#heading]]`：
  // 先拆出笔记部分做常规导航（纯 key 调用方不受影响），heading 部分经 pendingAnchor
  // 在目标笔记加载后滚动到对应标题行；同笔记纯锚点直接滚动不切换。
  const handleNavigate = useCallback(
    async (nextKey: string) => {
      const [notePart, heading] = parseWikilinkTarget(nextKey);
      if (heading !== null && (notePart === "" || notePart === selectedKey)) {
        // 同笔记内锚点：不切换笔记，直接滚动到标题行
        try {
          await editorRef.current?.scrollToHeading(heading);
        } catch {
          // 标题不存在时保持现状（scrollToHeading 内部静默）
        }
        return;
      }
      try {
        await editorRef.current?.flushSave();
      } catch {
        if (!window.confirm("保存失败，仍要离开吗？未保存的改动将丢失。")) return;
      }
      if (heading !== null) setPendingAnchor({ key: notePart, heading });
      else setPendingAnchor(null);
      navigate(notePart);
    },
    [navigate, selectedKey],
  );

  const handleBack = useCallback(async () => {
    try {
      await editorRef.current?.flushSave();
    } catch {
      if (!window.confirm("保存失败，仍要离开吗？未保存的改动将丢失。")) return;
    }
    back();
  }, [back]);

  const handleForward = useCallback(async () => {
    try {
      await editorRef.current?.flushSave();
    } catch {
      if (!window.confirm("保存失败，仍要离开吗？未保存的改动将丢失。")) return;
    }
    forward();
  }, [forward]);

  const handleSaved = useCallback(() => {
    setRefreshToken((n) => n + 1);
    scheduleAutoSync();
  }, [scheduleAutoSync]);

  // Agent 触发的 vault 变更：同 handleSaved 路径（刷新 + 自动同步）
  const handleAgentVaultChanged = useCallback(() => {
    setRefreshToken((n) => n + 1);
    scheduleAutoSync();
  }, [scheduleAutoSync]);

  // 目标笔记加载完成后消费 pendingAnchor：滚动到锚点标题行。
  // load 失败（笔记不存在等）时 NoteEditor 不调 onNoteLoaded，pendingAnchor 保留
  // 到下一次成功加载——但若 key 已不匹配则丢弃，避免跳到无关笔记。
  const handleNoteLoaded = useCallback(
    (loadedKey: string) => {
      if (!pendingAnchor) return;
      if (pendingAnchor.key !== loadedKey) {
        setPendingAnchor(null);
        return;
      }
      setPendingAnchor(null);
      // CM 视图挂载/布局需要一帧：rAF 后再滚动
      requestAnimationFrame(() => {
        editorRef.current?.scrollToHeading(pendingAnchor.heading);
      });
    },
    [pendingAnchor],
  );

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
      } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e);
        toast.error(msg);
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
      } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e);
        toast.error(msg);
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
      // 历史导航（输入框/编辑器内不触发；已 preventDefault 的事件也不劫持，
      // 避免劫持 CodeMirror 的按词跳转 Alt/Ctrl+←/→）
      if (e.defaultPrevented) return;
      const navTarget = e.target as HTMLElement | null;
      if (navTarget?.closest?.(".cm-editor, input, textarea, [contenteditable]")) return;
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

  // —— 以下回调专为 memo 子组件稳定身份：依赖仅为已稳定的 handle* ——
  // （位置必须在 handleManualSync/getCurrentNoteForAi/handleScrollToLine/handleAiInsert
  // 定义之后，避免 TDZ 引用）
  const handleSelectNote = useCallback((k: string) => void handleNavigate(k), [handleNavigate]);
  const handleCreateNoteFromSidebar = useCallback(
    (k: string, t: string) => void handleCreateNote(k, t),
    [handleCreateNote],
  );
  const handleBackVoid = useCallback(() => void handleBack(), [handleBack]);
  const handleForwardVoid = useCallback(() => void handleForward(), [handleForward]);
  const handleOpenSwitcher = useCallback(() => setSwitcherOpen(true), []);
  const handleCloseSwitcher = useCallback(() => setSwitcherOpen(false), []);
  const handleOpenSettings = useCallback(() => setSettingsOpen(true), []);
  const handleCloseSettings = useCallback(() => setSettingsOpen(false), []);
  const handleSyncNowVoid = useCallback(() => void handleManualSync(), [handleManualSync]);
  const handleFlushSave = useCallback(
    () => editorRef.current?.flushSave() ?? Promise.resolve(),
    [],
  );
  const handleNavigateVoid = useCallback((k: string) => void handleNavigate(k), [handleNavigate]);
  const handleDragPreviewLeft = useCallback((w: number) => handleDragPreview("left", w), [handleDragPreview]);
  const handleDragPreviewRight = useCallback((w: number) => handleDragPreview("right", w), [handleDragPreview]);
  const handleResolvedConflict = useCallback(
    (k: string) => {
      clearPendingConflict(k);
      bumpRefreshToken();
    },
    [clearPendingConflict, bumpRefreshToken],
  );
  const handleCloseConflicts = useCallback(() => {
    setConflictOpen(false);
    bumpRefreshToken();
  }, [bumpRefreshToken]);
  const handleCloseReport = useCallback(() => {
    setReportDialogOpen(false);
    setReport(null);
  }, [setReport]);

  // 拖拽中生效宽度：dragWidth 命中对应 side 时覆盖 layout（仅拖拽帧）
  const leftWidthLive = dragWidth?.side === "left" ? dragWidth.width : layout.leftWidth;
  const rightWidthLive = dragWidth?.side === "right" ? dragWidth.width : layout.rightWidth;

  // 面板元素 memo 化：依赖均为稳定回调 / 标量，还原 RightPanel 按需渲染。
  // 四面板中 AgentPanel 为重量级但带内部线程状态，经确认其全部状态
  // （view/items/queue/model 等）均为 React state（见 AgentPanel.tsx），
  // 卸载后重挂仅回到"新会话"占位、无后台线程可丢；且 RightPanel 非 agent tab
  // 时 agent 本就不在视图内。按任务要求仅 agent 面板 keepMounted，其余按 tab 按需渲染。
  const graphPanelEl = useMemo(
    () => (
      <GraphPanel
        selectedKey={selectedKey}
        refreshToken={refreshToken}
        onNavigate={handleNavigateVoid}
        onCreate={handleCreated}
      />
    ),
    [selectedKey, refreshToken, handleNavigateVoid, handleCreated],
  );
  const backlinksPanelEl = useMemo(
    () => (
      <BacklinksPanel
        selectedKey={selectedKey}
        refreshToken={refreshToken}
        onNavigate={handleNavigateVoid}
      />
    ),
    [selectedKey, refreshToken, handleNavigateVoid],
  );
  const tocPanelEl = useMemo(
    () => (
      <TocPanel
        noteKey={selectedKey}
        getCurrentNote={getCurrentNoteForAi}
        refreshToken={refreshToken}
        onScrollToLine={handleScrollToLine}
      />
    ),
    [selectedKey, getCurrentNoteForAi, refreshToken, handleScrollToLine],
  );
  const agentPanelEl = useMemo(
    () => (
      <Suspense
        fallback={
          <p className="px-3 py-6 text-center text-xs text-muted-foreground">
            Agent 面板加载中…
          </p>
        }
      >
        <AgentPanel
          onInsertToNote={handleAiInsert}
          vaultRoot={vault?.root ?? null}
          onVaultChanged={handleAgentVaultChanged}
          flushSave={handleFlushSave}
          onOpenSettings={handleOpenSettings}
          noteKey={selectedKey}
          getCurrentNote={getCurrentNoteForAi}
        />
      </Suspense>
    ),
    [
      handleAiInsert,
      vault?.root,
      handleAgentVaultChanged,
      handleFlushSave,
      handleOpenSettings,
      selectedKey,
      getCurrentNoteForAi,
    ],
  );
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
        onBack={handleBackVoid}
        onForward={handleForwardVoid}
        onOpenSwitcher={handleOpenSwitcher}
        onOpenSettings={handleOpenSettings}
        syncing={syncing}
        syncStatus={syncStatus}
        onSyncNow={handleSyncNowVoid}
        leftVisible={!layout.leftCollapsed}
        rightVisible={!layout.rightCollapsed}
        onToggleLeft={toggleLeft}
        onToggleRight={toggleRight}
      />

      <div className="flex min-h-0 flex-1">
        <ResizableAside
          side="left"
          width={leftWidthLive}
          collapsed={layout.leftCollapsed}
          dragging={draggingSide === "left"}
          extraClassName=""
          contentClassName="h-full w-full overflow-hidden"
          label="左侧栏"
          resizer={
            <PanelResizer
              side="left"
              currentWidth={leftWidthLive}
              dragging={draggingSide === "left"}
              onDrag={handleDragPreviewLeft}
              onDragStart={handleDragStartLeft}
              onDragEnd={handleDragEndLeft}
              onReset={handleResetLeft}
            />
          }
        >
          <WikiSidebar
            selectedKey={selectedKey}
            onSelect={handleSelectNote}
            refreshToken={refreshToken}
            vaultInfo={vault}
            onCreateNote={handleCreateNoteFromSidebar}
            onFolderChanged={handleFolderChanged}
            onHistoryReplacePrefix={handleHistoryReplacePrefix}
            onHistoryRemovePrefix={handleHistoryRemovePrefix}
          />
        </ResizableAside>

        <main className="min-w-0 flex-1 overflow-hidden bg-background">
          <NoteEditor
            key={selectedKey ?? "__none__"}
            ref={editorRef}
            noteKey={selectedKey}
            onSaved={handleSaved}
            onDeleted={handleDeleted}
            onDirtyChange={setEditorDirty}
            onNavigate={handleNavigateVoid}
            onCreate={handleCreated}
            onRenamed={handleRenamed}
            onOpenSettings={handleOpenSettings}
            refreshToken={refreshToken}
            onNoteLoaded={handleNoteLoaded}
          />
        </main>

        <ResizableAside
          side="right"
          width={rightWidthLive}
          collapsed={layout.rightCollapsed}
          dragging={draggingSide === "right"}
          extraClassName="hidden xl:flex xl:flex-col"
          contentClassName="flex h-full w-full flex-col overflow-hidden"
          label="右侧栏"
          resizer={
            <PanelResizer
              side="right"
              currentWidth={rightWidthLive}
              dragging={draggingSide === "right"}
              onDrag={handleDragPreviewRight}
              onDragStart={handleDragStartRight}
              onDragEnd={handleDragEndRight}
              onReset={handleResetRight}
            />
          }
        >
          <RightPanel
            graphPanel={graphPanelEl}
            backlinksPanel={backlinksPanelEl}
            tocPanel={tocPanelEl}
            agentPanel={agentPanelEl}
          />
        </ResizableAside>
      </div>
      <QuickSwitcher
        open={switcherOpen}
        onClose={handleCloseSwitcher}
        onSelect={handleSelectNote}
        onCreate={handleCreateNoteFromSidebar}
      />
      {settingsOpen && <SettingsDialog onClose={handleCloseSettings} onSync={handleSyncNowVoid} />}
      {conflictOpen && pendingConflicts.length > 0 && syncState && (
        <Suspense
          fallback={
            <p className="px-3 py-6 text-center text-xs text-muted-foreground">
              冲突解决面板加载中…
            </p>
          }
        >
          <ConflictResolveDialog
            conflicts={pendingConflicts}
            state={syncState}
            onResolved={handleResolvedConflict}
            onClose={handleCloseConflicts}
          />
        </Suspense>
      )}
      {reportDialogOpen && report && !(conflictOpen && pendingConflicts.length > 0) && (
        <SyncReportDialog
          report={report}
          onClose={handleCloseReport}
          onNavigate={handleNavigateVoid}
        />
      )}
      <ToastHost />
    </div>
  );
}
