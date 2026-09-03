import { useCallback, useEffect, useState } from "react";
import { WikiSidebar } from "@/components/WikiSidebar";
import { NoteEditor } from "@/components/NoteEditor";
import { GraphPanel } from "@/components/GraphPanel";
import { vaultInfo } from "@/api/tauri";
import type { VaultInfo } from "@/api/types";

export default function App() {
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [refreshToken, setRefreshToken] = useState(0);
  const [vault, setVault] = useState<VaultInfo | null>(null);
  const [editorDirty, setEditorDirty] = useState(false);

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

  const handleSelect = useCallback(
    (nextKey: string) => {
      if (editorDirty) {
        const ok = window.confirm("有未保存的改动，确定要切换笔记吗？未保存的内容会丢失。");
        if (!ok) return;
      }
      setSelectedKey(nextKey);
      setEditorDirty(false);
    },
    [editorDirty],
  );

  const handleSaved = useCallback(() => {
    setRefreshToken((n) => n + 1);
  }, []);

  const handleDeleted = useCallback(() => {
    setSelectedKey(null);
    setEditorDirty(false);
    setRefreshToken((n) => n + 1);
  }, []);

  return (
    <div className="flex h-screen flex-col bg-background">
      <header className="flex h-14 shrink-0 items-center border-b px-4">
        <h1 className="text-base font-semibold tracking-tight">Wiki Desktop</h1>
        <span className="ml-3 text-xs text-muted-foreground">本地离线 Wiki</span>
      </header>

      <div className="flex min-h-0 flex-1">
        <aside className="w-[300px] shrink-0 border-r bg-sidebar max-[900px]:w-[260px]">
          <WikiSidebar
            selectedKey={selectedKey}
            onSelect={handleSelect}
            refreshToken={refreshToken}
            vaultInfo={vault}
          />
        </aside>

        <main className="min-w-0 flex-1 overflow-y-auto bg-background">
          <NoteEditor
            key={selectedKey ?? "__none__"}
            noteKey={selectedKey}
            onSaved={handleSaved}
            onDeleted={handleDeleted}
            onDirtyChange={setEditorDirty}
          />
        </main>

        <aside className="hidden w-[320px] shrink-0 border-l bg-sidebar xl:block">
          <GraphPanel selectedKey={selectedKey} refreshToken={refreshToken} />
        </aside>
      </div>
    </div>
  );
}
