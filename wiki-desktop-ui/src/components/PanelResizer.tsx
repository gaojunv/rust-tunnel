import { useCallback, useEffect, useRef } from "react";

type Props = {
  side: "left" | "right";
  currentWidth: number;
  onDrag: (width: number) => void;
  onDragEnd: () => void;
  onReset: () => void;
  dragging?: boolean;
};

export function PanelResizer({ side, currentWidth, onDrag, onDragEnd, onReset, dragging = false }: Props) {
  const draggingRef = useRef(false);
  const startXRef = useRef(0);
  const startWidthRef = useRef(currentWidth);

  // 同步最新 currentWidth，供 pointermove 使用（避免闭包过期）
  const currentWidthRef = useRef(currentWidth);
  useEffect(() => {
    currentWidthRef.current = currentWidth;
  }, [currentWidth]);

  const onDragRef = useRef(onDrag);
  const onDragEndRef = useRef(onDragEnd);
  useEffect(() => {
    onDragRef.current = onDrag;
  }, [onDrag]);
  useEffect(() => {
    onDragEndRef.current = onDragEnd;
  }, [onDragEnd]);

  const cleanupBody = useCallback(() => {
    document.body.style.removeProperty("user-select");
    document.body.style.removeProperty("cursor");
  }, []);

  useEffect(() => {
    return () => cleanupBody();
  }, [cleanupBody]);

  const handlePointerDown = useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      // 仅主指针
      if (e.button !== 0) return;
      e.preventDefault();
      draggingRef.current = true;
      startXRef.current = e.clientX;
      startWidthRef.current = currentWidthRef.current;
      document.body.style.userSelect = "none";
      document.body.style.cursor = "col-resize";
      const target = e.currentTarget;
      try {
        target.setPointerCapture(e.pointerId);
      } catch {
        // 某些环境不支持
      }

      const handleMove = (ev: PointerEvent) => {
        if (!draggingRef.current) return;
        const dx = ev.clientX - startXRef.current;
        const next = startWidthRef.current + (side === "left" ? dx : -dx);
        onDragRef.current(next);
      };

      const handleUp = (ev: PointerEvent) => {
        if (!draggingRef.current) return;
        draggingRef.current = false;
        cleanupBody();
        try {
          target.releasePointerCapture(ev.pointerId);
        } catch {
          // ignore
        }
        window.removeEventListener("pointermove", handleMove);
        window.removeEventListener("pointerup", handleUp);
        window.removeEventListener("pointercancel", handleUp);
        onDragEndRef.current();
      };

      window.addEventListener("pointermove", handleMove);
      window.addEventListener("pointerup", handleUp);
      window.addEventListener("pointercancel", handleUp);
    },
    [side, cleanupBody],
  );

  return (
    <div
      role="separator"
      aria-orientation="vertical"
      onPointerDown={handlePointerDown}
      onDoubleClick={(e) => {
        e.preventDefault();
        onReset();
      }}
      className="group absolute top-0 z-10 h-full w-[6px] cursor-col-resize touch-none"
      style={side === "left" ? { right: -3 } : { left: -3 }}
      title="拖拽调整宽度，双击恢复默认"
    >
      {/* 指示线：pointer-events-none 自身不触发 hover，用 group-hover 跟随手柄 */}
      <div
        className={`pointer-events-none absolute inset-y-0 left-1/2 w-0.5 -translate-x-1/2 transition-colors ${
          dragging ? "bg-primary" : "bg-transparent group-hover:bg-border"
        }`}
      />
    </div>
  );
}
