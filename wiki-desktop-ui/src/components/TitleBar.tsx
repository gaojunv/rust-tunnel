import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AlertCircle,
  Check,
  ChevronLeft,
  ChevronRight,
  CloudOff,
  Loader2,
  Maximize2,
  Minus,
  Search,
  Settings,
  Square,
  X,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { ThemeToggle } from "@/components/ThemeToggle";
import { isTauri } from "@/api/tauri";
import type { SyncStatus } from "@/lib/use-sync";

type Props = {
  dirty?: boolean;
  canBack?: boolean;
  canForward?: boolean;
  onBack?: () => void;
  onForward?: () => void;
  onOpenSwitcher?: () => void;
  onOpenSettings?: () => void;
  syncing?: boolean;
  syncStatus?: SyncStatus;
  onSyncNow?: () => void;
};

function isMac(): boolean {
  return typeof navigator !== "undefined" && navigator.userAgent.includes("Mac OS X");
}

function formatLastSync(lastSyncAt: number | null): string {
  if (!lastSyncAt) return "尚未同步";
  const diff = Date.now() - lastSyncAt;
  if (diff < 60_000) return "刚刚同步";
  const mins = Math.floor(diff / 60_000);
  if (mins < 60) return `上次同步 ${mins} 分钟前`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `上次同步 ${hours} 小时前`;
  const days = Math.floor(hours / 24);
  return `上次同步 ${days} 天前`;
}

function SyncWidget({ status, onSyncNow }: { status: SyncStatus; onSyncNow?: () => void }) {
  const tooltip = useMemo(() => {
    switch (status.phase) {
      case "syncing":
        return "同步中…";
      case "offline":
        return "离线，将自动重试";
      case "error":
        return status.lastError ?? "同步失败";
      case "idle": {
        if (status.pendingCount != null && status.pendingCount > 0) return `${status.pendingCount} 篇待上传`;
        return formatLastSync(status.lastSyncAt);
      }
      default:
        return "";
    }
  }, [status]);

  if (status.phase === "disabled") return null;

  if (status.phase === "syncing") {
    return (
      <Button
        type="button"
        variant="ghost"
        size="icon"
        className="size-8"
        disabled
        aria-label="同步中"
        title={tooltip}
      >
        <Loader2 className="size-4 animate-spin" />
      </Button>
    );
  }

  if (status.phase === "offline") {
    return (
      <Button
        type="button"
        variant="ghost"
        size="icon"
        className="size-8"
        onClick={onSyncNow}
        aria-label="离线"
        title={tooltip}
      >
        <CloudOff className="size-4 text-muted-foreground" />
      </Button>
    );
  }

  if (status.phase === "error") {
    return (
      <Button
        type="button"
        variant="ghost"
        size="icon"
        className="size-8"
        onClick={onSyncNow}
        aria-label="同步失败"
        title={tooltip}
      >
        <AlertCircle className="size-4 text-amber-500" />
      </Button>
    );
  }

  // idle
  if (status.pendingCount != null && status.pendingCount > 0) {
    return (
      <Button
        type="button"
        variant="ghost"
        size="icon"
        className="relative size-8"
        onClick={onSyncNow}
        aria-label={`${status.pendingCount} 篇待上传`}
        title={tooltip}
      >
        <CloudOff className="size-4 text-amber-500" />
        <span className="absolute -right-0.5 -top-0.5 flex min-w-[14px] justify-center rounded-full bg-amber-500 px-1 text-[10px] font-medium leading-none text-white">
          {status.pendingCount > 99 ? "99+" : String(status.pendingCount)}
        </span>
      </Button>
    );
  }

  return (
    <Button
      type="button"
      variant="ghost"
      size="icon"
      className="size-8"
      onClick={onSyncNow}
      aria-label="已同步"
      title={tooltip}
    >
      <Check className="size-4 text-green-600" />
    </Button>
  );
}

export function TitleBar({
  dirty = false,
  canBack = false,
  canForward = false,
  onBack,
  onForward,
  onOpenSwitcher,
  onOpenSettings,
  syncing = false,
  syncStatus,
  onSyncNow,
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

  const syncingEffective = syncing || syncStatus?.phase === "syncing";

  return (
    <div
      className="flex h-10 shrink-0 select-none items-center border-b border-border bg-background"
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
        {syncStatus && <SyncWidget status={syncStatus} onSyncNow={onSyncNow} />}
        {onOpenSettings && (
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="size-8"
            onClick={onOpenSettings}
            aria-label="同步设置"
            title="同步设置"
          >
            <Settings className="size-4" />
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
      {syncingEffective && !syncStatus && <span className="sr-only">同步中</span>}
    </div>
  );
}
