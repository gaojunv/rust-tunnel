/**
 * 排队消息 chip 列表 —— 显示在输入框上方
 * 每条：截断文本 + 删除按钮；空队列不渲染。
 * 样式对齐面板既有 chip（笔记上下文 chip）：
 * rounded-full border bg-muted/60 + 等宽小字。
 */
import { MessageSquare, X } from "lucide-react";
import type { QueueItem } from "@/lib/agent/queue";

type QueuedChipsProps = {
  items: QueueItem[];
  /** 删除单条（按 id） */
  onRemove: (id: string) => void;
};

export function QueuedChips({ items, onRemove }: QueuedChipsProps) {
  if (items.length === 0) return null;

  return (
    <div className="mx-2 mb-1 flex flex-wrap items-center gap-1.5">
      {items.map((it) => (
        <span
          key={it.id}
          className="inline-flex max-w-full items-center gap-1.5 rounded-full border border-border/60 bg-muted/60 px-2.5 py-1 text-[11px] text-muted-foreground"
          title={it.text}
        >
          <MessageSquare className="size-3 shrink-0" />
          <span className="max-w-[13rem] truncate font-mono">{it.text}</span>
          <button
            type="button"
            onClick={() => onRemove(it.id)}
            className="ml-0.5 shrink-0 rounded-full p-0.5 hover:bg-accent hover:text-foreground"
            aria-label="从队列移除"
            title="移出队列"
          >
            <X className="size-3" />
          </button>
        </span>
      ))}
      {items.length > 1 && <span className="shrink-0 text-[10px] text-muted-foreground/70">共 {items.length} 条</span>}
    </div>
  );
}
