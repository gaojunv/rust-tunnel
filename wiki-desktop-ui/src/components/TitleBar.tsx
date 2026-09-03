import { useCallback, useEffect, useState } from "react";
import { ChevronLeft, ChevronRight, Maximize2, Minus, Search, Square, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ThemeToggle } from "@/components/ThemeToggle";
import { isTauri } from "@/api/tauri";

type Props = {
  dirty?: boolean;
  canBack?: boolean;
  canForward?: boolean;
  onBack?: () => void;
  onForward?: () => void;
  onOpenSwitcher?: () => void;
};

function isMac(): boolean {
  return typeof navigator !== "undefined" && navigator.userAgent.includes("Mac OS X");
}

export function TitleBar({
  dirty = false,
  canBack = false,
  canForward = false,
  onBack,
  onForward,
  onOpenSwitcher,
}: Props) {
  const [maximized, setMaximized] = useState(false);
  const showWindowControls = isTauri;
  const mac = isMac();

  const refreshMaximized = useCallback(async () => {
    if (!isTauri) return;
    try {
      const { getCurrentWindow } = await import("@tauri-apps/api/window");
      const win = getCurrentWindow();
      const v = await win.isMaximized();
      setMaximized(v);
    } catch {
      // ignore
    }
  }, []);

  useEffect(() => {
    if (!isTauri) return;
    void refreshMaximized();
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    (async () => {
      try {
        const { getCurrentWindow } = await import("@tauri-apps/api/window");
        const win = getCurrentWindow();
        const fn = await win.onResized(() => {
          void refreshMaximized();
        });
        if (cancelled) {
          fn();
        } else {
          unlisten = fn;
        }
      } catch {
        // ignore
      }
    })();
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [refreshMaximized]);

  const handleMinimize = useCallback(async () => {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    await getCurrentWindow().minimize();
  }, []);

  const handleToggleMaximize = useCallback(async () => {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    await getCurrentWindow().toggleMaximize();
    // onResized will sync state; optimistically flip afterwards
    void refreshMaximized();
  }, [refreshMaximized]);

  const handleClose = useCallback(async () => {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    await getCurrentWindow().close();
  }, []);

  const handleMouseDown = useCallback(async (e: React.MouseEvent) => {
    if (e.button !== 0) return;
    const target = e.target as HTMLElement;
    if (target.closest("button, a, input")) return;
    if (!isTauri) return;
    try {
      const { getCurrentWindow } = await import("@tauri-apps/api/window");
      await getCurrentWindow().startDragging();
    } catch {
      // ignore
    }
  }, []);

  const handleDoubleClick = useCallback(
    async (e: React.MouseEvent) => {
      const target = e.target as HTMLElement;
      if (target.closest("button, a, input")) return;
      if (!isTauri) return;
      await handleToggleMaximize();
    },
    [handleToggleMaximize],
  );

  return (
    <div
      className="flex h-10 shrink-0 select-none items-center border-b border-border bg-background"
      // Manual dragging via startDragging() — do NOT use data-tauri-drag-region:
      // the injected region listener captures bubbled mousedown from child buttons
      // and causes mis-drags when clicking window controls.
      onMouseDown={handleMouseDown}
      onDoubleClick={handleDoubleClick}
    >
      {/* Left cluster */}
      <div className={`flex items-center gap-0.5 ${mac ? "pl-[76px]" : "pl-2"}`}>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="size-8"
          disabled={!canBack}
          onClick={onBack}
          aria-label="后退"
          title="后退"
        >
          <ChevronLeft className="size-4" />
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="size-8"
          disabled={!canForward}
          onClick={onForward}
          aria-label="前进"
          title="前进"
        >
          <ChevronRight className="size-4" />
        </Button>
        <span className="ml-2 text-xs font-medium text-muted-foreground">Wiki Desktop</span>
        {dirty && (
          <span
            className="ml-1.5 size-2 rounded-full bg-amber-500"
            aria-label="有未保存的改动"
            title="有未保存的改动"
          />
        )}
      </div>

      <div className="flex-1" />

      {/* Right cluster */}
      <div className="flex items-center gap-0 pr-1">
        {onOpenSwitcher && (
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="size-8"
            onClick={onOpenSwitcher}
            aria-label="快速切换"
            title="快速切换 (Ctrl+K)"
          >
            <Search className="size-4" />
          </Button>
        )}
        <ThemeToggle />
        {showWindowControls && (
          <>
            <Button
              type="button"
              variant="ghost"
              size="icon"
              className="size-8 rounded-none hover:bg-accent"
              onClick={handleMinimize}
              aria-label="最小化"
              title="最小化"
            >
              <Minus className="size-4" />
            </Button>
            <Button
              type="button"
              variant="ghost"
              size="icon"
              className="size-8 rounded-none hover:bg-accent"
              onClick={handleToggleMaximize}
              aria-label={maximized ? "还原" : "最大化"}
              title={maximized ? "还原" : "最大化"}
            >
              {maximized ? <Square className="size-3.5" /> : <Maximize2 className="size-4" />}
            </Button>
            <Button
              type="button"
              variant="ghost"
              size="icon"
              className="size-8 rounded-none hover:bg-accent hover:bg-destructive hover:text-destructive-foreground"
              onClick={handleClose}
              aria-label="关闭"
              title="关闭"
            >
              <X className="size-4" />
            </Button>
          </>
        )}
      </div>
    </div>
  );
}
