import { useEffect } from "react";
import { createPortal } from "react-dom";
import { Button } from "@/components/ui/button";
import type { SyncReport } from "@/lib/sync-engine";
import {
  Upload,
  Download,
  AlertTriangle,
  RotateCcw,
  Trash2,
  Minus,
  EyeOff,
  XCircle,
  CheckCircle2,
} from "lucide-react";

/**
 * 同步结果报告对话框 —— 展示 SyncReport 计数与明细
 * 冲突项点击可跳转笔记（含 .conflict- 副本 key）
 */

type Props = {
  report: SyncReport;
  onClose: () => void;
  onNavigate: (key: string) => void;
};

// 动作图标映射
function ActionIcon({ kind }: { kind: string }) {
  switch (kind) {
    case "upload":
      return <Upload className="size-4 shrink-0 text-blue-600" />;
    case "download":
      return <Download className="size-4 shrink-0 text-green-600" />;
    case "conflict-local-wins":
    case "conflict-remote-wins":
    case "conflict-pending":
      return <AlertTriangle className="size-4 shrink-0 text-amber-600" />;
    case "restore-remote":
      return <RotateCcw className="size-4 shrink-0 text-purple-600" />;
    case "delete-remote":
      return <Trash2 className="size-4 shrink-0 text-red-600" />;
    case "drop-state":
      return <Minus className="size-4 shrink-0 text-muted-foreground" />;
    case "skip-incompatible":
    case "skip-empty":
    case "skip-conflict-copy":
      return <EyeOff className="size-4 shrink-0 text-muted-foreground" />;
    default:
      return <XCircle className="size-4 shrink-0 text-muted-foreground" />;
  }
}

// 动作中文标签
function kindLabel(kind: string): string {
  switch (kind) {
    case "upload":
      return "上传";
    case "download":
      return "下载";
    case "conflict-local-wins":
      return "冲突（本地胜）";
    case "conflict-remote-wins":
      return "冲突（远端胜）";
    case "conflict-pending":
      return "冲突（待解决）";
    case "restore-remote":
      return "恢复远端";
    case "delete-remote":
      return "删除远端";
    case "drop-state":
      return "清理状态";
    case "skip-incompatible":
      return "跳过（不兼容）";
    case "skip-empty":
      return "跳过（空内容）";
    case "skip-conflict-copy":
      return "跳过（冲突副本）";
    default:
      return kind;
  }
}

export function SyncReportDialog({ report, onClose, onNavigate }: Props) {
  // Esc 关闭
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const handleNavigate = (key: string) => {
    onClose();
    onNavigate(key);
  };

  const overlay = (
    <div
      data-modal-open=""
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="flex max-h-[80vh] w-[min(92vw,560px)] flex-col rounded-lg border border-border bg-popover shadow-xl"
        onMouseDown={(e) => e.stopPropagation()}
        onClick={(e) => e.stopPropagation()}
      >
        {/* 头部 */}
        <div className="border-b px-4 py-3">
          <h2 className="text-sm font-semibold">同步报告</h2>
          {report.conflicts > 0 && (
            <p className="mt-1 text-xs text-amber-600">{report.conflicts} 个冲突待解决</p>
          )}
          {/* 计数行 */}
          <div className="mt-2 flex flex-wrap gap-2 text-xs">
            <span className="rounded bg-blue-100 px-2 py-0.5 text-blue-700 dark:bg-blue-900 dark:text-blue-200">
              上传 {report.uploaded}
            </span>
            <span className="rounded bg-green-100 px-2 py-0.5 text-green-700 dark:bg-green-900 dark:text-green-200">
              下载 {report.downloaded}
            </span>
            <span className="rounded bg-amber-100 px-2 py-0.5 text-amber-700 dark:bg-amber-900 dark:text-amber-200">
              冲突 {report.conflicts}
            </span>
            <span className="rounded bg-purple-100 px-2 py-0.5 text-purple-700 dark:bg-purple-900 dark:text-purple-200">
              恢复 {report.restored}
            </span>
            <span className="rounded bg-red-100 px-2 py-0.5 text-red-700 dark:bg-red-900 dark:text-red-200">
              删除 {report.deletedRemote}
            </span>
            <span className="rounded bg-muted px-2 py-0.5 text-muted-foreground">跳过 {report.skipped}</span>
            <span className="rounded bg-destructive/10 px-2 py-0.5 text-destructive">错误 {report.errors}</span>
          </div>
        </div>

        {/* 明细列表 */}
        <div className="min-h-0 flex-1 overflow-auto px-4 py-2">
          {report.items.length === 0 ? (
            <p className="py-6 text-center text-sm text-muted-foreground">无需同步，已是最新</p>
          ) : (
            <ul className="space-y-1.5">
              {report.items.map((item, idx) => {
                const isConflict =
                  item.action.kind === "conflict-local-wins" || item.action.kind === "conflict-remote-wins" || item.action.kind === "conflict-pending";
                // 跳过类与普通项：key 可点击（含 .conflict- 副本 key 同样可点击）
                const keyClickable = true;
                return (
                  <li
                    key={`${item.action.kind}-${item.action.key}-${idx}`}
                    className="flex items-start gap-2 rounded-md border px-2 py-1.5 text-xs"
                  >
                    {item.ok ? (
                      <CheckCircle2 className="mt-0.5 size-3.5 shrink-0 text-green-500" />
                    ) : (
                      <XCircle className="mt-0.5 size-3.5 shrink-0 text-destructive" />
                    )}
                    <ActionIcon kind={item.action.kind} />
                    <div className="min-w-0 flex-1">
                      <div className="flex flex-wrap items-center gap-1">
                        <span className="rounded bg-muted px-1 py-0.5 text-[11px]">{kindLabel(item.action.kind)}</span>
                        <button
                          type="button"
                          onClick={() => keyClickable && handleNavigate(item.action.key)}
                          className="max-w-[260px] truncate rounded px-1 py-0.5 text-left font-mono text-xs hover:bg-accent hover:text-accent-foreground"
                          title={`跳转到 ${item.action.key}`}
                        >
                          {item.action.key}
                        </button>
                      </div>
                      {item.detail && <p className="mt-0.5 break-all text-[11px] text-muted-foreground">{item.detail}</p>}
                      {/* 不兼容原因展示 */}
                      {item.action.kind === "skip-incompatible" && (
                        <p className="mt-0.5 break-all text-[11px] text-muted-foreground">{item.action.reason}</p>
                      )}
                      {/* 冲突项说明：点击可跳转 */}
                      {isConflict && (
                        <p className="mt-0.5 text-[11px] text-amber-600">点击 key 跳转查看笔记</p>
                      )}
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
        </div>

        <div className="flex justify-end border-t px-4 py-3">
          <Button type="button" onClick={onClose}>
            关闭
          </Button>
        </div>
      </div>
    </div>
  );

  return createPortal(overlay, document.body);
}
