// 通用右键上下文菜单：定位在鼠标处，点击外部/Esc/滚动关闭，自动避让视口边缘
import { useLayoutEffect, useRef } from "react";
import { createPortal } from "react-dom";
import { cn } from "@/lib/utils";

export type ContextMenuProps = {
  /** 菜单左上角位置（viewport 坐标，通常取自 MouseEvent.clientX/Y） */
  x: number;
  y: number;
  onClose: () => void;
  children: React.ReactNode;
};

export function ContextMenu({ x, y, onClose, children }: ContextMenuProps) {
  const ref = useRef<HTMLDivElement>(null);

  // 初次渲染后量尺寸，超出视口时向左/上翻转
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    const margin = 8;
    const dx = Math.min(0, window.innerWidth - rect.width - margin - rect.left);
    const dy = Math.min(0, window.innerHeight - rect.height - margin - rect.top);
    if (dx !== 0 || dy !== 0) {
      el.style.transform = `translate(${dx}px, ${dy}px)`;
    }
  }, []);

  // 点击外部 / Esc / 滚动 / 窗口变化时关闭
  useLayoutEffect(() => {
    const onPointerDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    const onScroll = () => onClose();
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    // capture 阶段监听，任意祖先滚动（侧栏 ScrollArea 等）都关闭菜单
    document.addEventListener("scroll", onScroll, true);
    window.addEventListener("resize", onClose);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
      document.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", onClose);
    };
  }, [onClose]);

  return createPortal(
    <div
      ref={ref}
      role="menu"
      data-testid="context-menu"
      className="fixed z-50 min-w-[180px] rounded-lg border border-border bg-popover p-1 shadow-xl"
      style={{ left: x, top: y }}
      onContextMenu={(e) => e.preventDefault()}
    >
      {children}
    </div>,
    document.body,
  );
}

export type ContextMenuItemProps = {
  label: string;
  icon?: React.ReactNode;
  /** 危险项（删除等）：红色文字 + 红色 hover 底 */
  danger?: boolean;
  onSelect: () => void;
};

export function ContextMenuItem({ label, icon, danger, onSelect }: ContextMenuItemProps) {
  return (
    <button
      type="button"
      role="menuitem"
      onClick={onSelect}
      className={cn(
        "flex w-full items-center gap-2 rounded-md px-2.5 py-1.5 text-left text-sm transition-colors",
        danger ? "text-destructive hover:bg-destructive/10" : "hover:bg-accent",
      )}
    >
      {icon}
      <span className="truncate">{label}</span>
    </button>
  );
}
