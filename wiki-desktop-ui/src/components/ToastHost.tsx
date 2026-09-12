// 全局 toast 容器：固定右下角堆叠，自动消失 + 手动关闭。
// 配色沿用 shadcn 语义 token（border-border / bg-popover），参考 ContextMenu.tsx。
import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { CircleAlert, CircleCheck, Info, X } from "lucide-react";
import { cn } from "@/lib/utils";
import { dismissToast, subscribeToast, type ToastItem } from "@/lib/toast";

const KIND_STYLE: Record<ToastItem["kind"], { icon: typeof Info; iconClass: string; label: string }> = {
  success: { icon: CircleCheck, iconClass: "text-green-500", label: "成功" },
  error: { icon: CircleAlert, iconClass: "text-destructive", label: "错误" },
  info: { icon: Info, iconClass: "text-muted-foreground", label: "提示" },
};

function ToastCard({ item }: { item: ToastItem }) {
  const [expanded, setExpanded] = useState(false);
  const style = KIND_STYLE[item.kind];
  const Icon = style.icon;
  // error 内容可能很长：默认 3 行截断，点击正文切换展开/折叠
  const long = item.kind === "error" && item.message.length > 120;
  return (
    <div
      role="status"
      aria-live={item.kind === "error" ? "assertive" : "polite"}
      data-testid="toast-item"
      data-kind={item.kind}
      className={cn(
        "pointer-events-auto flex w-[min(92vw,360px)] items-start gap-2.5 rounded-lg border border-border",
        "bg-popover px-3 py-2.5 text-popover-foreground shadow-xl",
        "animate-in slide-in-from-right fade-in duration-200",
      )}
    >
      <Icon className={cn("mt-0.5 size-4 shrink-0", style.iconClass)} aria-label={style.label} />
      <button
        type="button"
        title={expanded ? "点击折叠" : "点击展开"}
        onClick={() => {
          if (long) setExpanded((v) => !v);
        }}
        className={cn(
          "min-w-0 flex-1 text-left text-sm leading-snug break-words",
          long && !expanded ? "line-clamp-3" : "max-h-40 overflow-y-auto whitespace-pre-wrap",
        )}
      >
        {item.message}
      </button>
      <button
        type="button"
        aria-label="关闭通知"
        onClick={() => dismissToast(item.id)}
        className="shrink-0 rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-accent-foreground"
      >
        <X className="size-3.5" />
      </button>
    </div>
  );
}

export function ToastHost() {
  const [items, setItems] = useState<ToastItem[]>([]);
  useEffect(() => subscribeToast(setItems), []);
  if (items.length === 0) return null;
  return createPortal(
    <div
      data-testid="toast-host"
      className="pointer-events-none fixed bottom-4 right-4 z-[60] flex flex-col items-end gap-2"
    >
      {items.map((item) => (
        <ToastCard key={item.id} item={item} />
      ))}
    </div>,
    document.body,
  );
}
