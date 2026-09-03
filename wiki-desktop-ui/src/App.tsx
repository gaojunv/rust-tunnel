import { useCallback, useEffect, useState } from "react";
import { TitleBar } from "@/components/TitleBar";
import { WikiSidebar } from "@/components/WikiSidebar";
import { NoteEditor } from "@/components/NoteEditor";
import { GraphPanel } from "@/components/GraphPanel";
import { saveNote, vaultInfo } from "@/api/tauri";
import type { VaultInfo } from "@/api/types";
import { useNoteHistory } from "@/lib/use-note-history";
import { QuickSwitcher } from "@/components/QuickSwitcher";

export default function App() {
  const { current: selectedKey, canBack, canForward, navigate, back, forward, remove, replace } =
    useNoteHistory();
  const [mode, setMode] = useState<"edit" | "preview">("preview");
  const [refreshToken, setRefreshToken] = useState(0);
  const [vault, setVault] = useState<VaultInfo | null>(null);
  const [editorDirty, setEditorDirty] = useState(false);
  const [switcherOpen, setSwitcherOpen] = useState(false);

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

  // 全局导航快捷键：Alt+ArrowLeft/Right 及 Ctrl+ArrowLeft/Right（!metaKey）
  // Ctrl+K/Ctrl+P：快速切换（toggle；重命名等其他模态打开时守卫优先，不抢焦点）
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

  return (
    <div className="flex h-screen flex-col bg-background">
      <TitleBar
        dirty={editorDirty}
        canBack={canBack}
        canForward={canForward}
        onBack={handleBack}
        onForward={handleForward}
        onOpenSwitcher={() => setSwitcherOpen(true)}
      />

      <div className="flex min-h-0 flex-1">
        <aside className="w-[300px] shrink-0 border-r bg-sidebar max-[900px]:w-[260px]">
          <WikiSidebar
            selectedKey={selectedKey}
            onSelect={handleNavigate}
            refreshToken={refreshToken}
            vaultInfo={vault}
            onCreateNote={handleCreateNote}
          />
        </aside>

        <main className="min-w-0 flex-1 overflow-hidden bg-background">
          <NoteEditor
            key={selectedKey ?? "__none__"}
            noteKey={selectedKey}
            mode={mode}
            onModeChange={setMode}
            onSaved={handleSaved}
            onDeleted={handleDeleted}
            onDirtyChange={setEditorDirty}
            onNavigate={handleNavigate}
            onCreate={handleCreated}
            onRenamed={handleRenamed}
          />
        </main>

        <aside className="hidden w-[320px] shrink-0 border-l bg-sidebar xl:block">
          <GraphPanel
            selectedKey={selectedKey}
            refreshToken={refreshToken}
            onNavigate={handleNavigate}
            onCreate={handleCreated}
          />
        </aside>
      </div>
      <QuickSwitcher
        open={switcherOpen}
        onClose={() => setSwitcherOpen(false)}
        onSelect={handleNavigate}
      />
    </div>
  );
}
