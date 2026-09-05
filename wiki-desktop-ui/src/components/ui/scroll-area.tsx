import * as React from "react";

import { cn } from "@/lib/utils";
import { thumbSize, thumbOffset, scrollOffsetFromThumb } from "@/lib/scrollbar-geometry";

type Orientation = "vertical" | "horizontal";

// 内部：根据 orientation 渲染一条 overlay 轨道 + 可拖拽 thumb
function ScrollbarTrack({
  containerEl,
  orientation,
}: {
  containerEl: HTMLElement | null;
  orientation: Orientation;
}) {
  const trackRef = React.useRef<HTMLDivElement | null>(null);
  const thumbRef = React.useRef<HTMLDivElement | null>(null);
  const [thumbLen, setThumbLen] = React.useState(0);
  const [thumbPos, setThumbPos] = React.useState(0);
  const [hovered, setHovered] = React.useState(false);
  const [scrolling, setScrolling] = React.useState(false);
  const [dragging, setDragging] = React.useState(false);

  const hideTimerRef = React.useRef<number | null>(null);
  const rafRef = React.useRef<number | null>(null);
  const prevUserSelectRef = React.useRef<string>("");
  const dragStateRef = React.useRef<{
    pointerOffset: number;
    trackStart: number;
  } | null>(null);

  const recalc = React.useCallback(() => {
    if (!containerEl) {
      setThumbLen(0);
      setThumbPos(0);
      return;
    }
    const client = orientation === "vertical" ? containerEl.clientHeight : containerEl.clientWidth;
    const scroll = orientation === "vertical" ? containerEl.scrollHeight : containerEl.scrollWidth;
    const len = thumbSize(client, scroll);
    setThumbLen(len);
    if (len === 0) {
      setThumbPos(0);
      return;
    }
    const off = orientation === "vertical" ? containerEl.scrollTop : containerEl.scrollLeft;
    setThumbPos(thumbOffset(off, client, scroll, len));
  }, [containerEl, orientation]);

  // 滚动监听：passive + rAF 节流；滚动发生时 1000ms 后淡出
  React.useEffect(() => {
    if (!containerEl) return;
    recalc();

    const onScroll = () => {
      if (rafRef.current !== null) return;
      rafRef.current = window.requestAnimationFrame(() => {
        rafRef.current = null;
        recalc();
        setScrolling(true);
        if (hideTimerRef.current !== null) window.clearTimeout(hideTimerRef.current);
        hideTimerRef.current = window.setTimeout(() => {
          setScrolling(false);
          hideTimerRef.current = null;
        }, 1000);
      });
    };

    containerEl.addEventListener("scroll", onScroll, { passive: true });

    // ResizeObserver 同时观察滚动元素与内容元素
    let ro: ResizeObserver | null = null;
    if (typeof ResizeObserver !== "undefined") {
      ro = new ResizeObserver(() => {
        // 内容尺寸变化时用 rAF 合并，避免抖动
        if (rafRef.current !== null) window.cancelAnimationFrame(rafRef.current);
        rafRef.current = window.requestAnimationFrame(() => {
          rafRef.current = null;
          recalc();
        });
      });
      ro.observe(containerEl);
      const content = containerEl.firstElementChild as Element | null;
      if (content) ro.observe(content);
    }

    // hover：监听容器 pointerenter/leave
    const onEnter = () => setHovered(true);
    const onLeave = () => setHovered(false);
    containerEl.addEventListener("pointerenter", onEnter);
    containerEl.addEventListener("pointerleave", onLeave);

    // 降级：ResizeObserver 不可用时，仅靠 scroll 事件更新

    return () => {
      containerEl.removeEventListener("scroll", onScroll);
      containerEl.removeEventListener("pointerenter", onEnter);
      containerEl.removeEventListener("pointerleave", onLeave);
      if (ro) ro.disconnect();
      if (rafRef.current !== null) window.cancelAnimationFrame(rafRef.current);
      if (hideTimerRef.current !== null) window.clearTimeout(hideTimerRef.current);
    };
  }, [containerEl, recalc]);

  // track 上的 hover 也保持可见（拖拽热区）
  const onTrackEnter = React.useCallback(() => setHovered(true), []);
  const onTrackLeave = React.useCallback(() => {
    // 仅在非拖拽时跟随容器 hover 语义
    if (!dragging) setHovered(false);
  }, [dragging]);

  // 拖拽：pointerdown 上捕获，move 用几何反映射更新 scrollTop/Left
  const onThumbPointerDown = React.useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      if (!containerEl || !trackRef.current || thumbLen === 0) return;
      // 只响应主指针
      if (e.button !== 0) return;
      e.preventDefault();
      const thumbEl = thumbRef.current;
      if (!thumbEl) return;
      const isVert = orientation === "vertical";
      const trackRect = trackRef.current.getBoundingClientRect();
      const thumbRect = thumbEl.getBoundingClientRect();
      const pointerOffset = (isVert ? e.clientY : e.clientX) - (isVert ? thumbRect.top : thumbRect.left);
      const trackStart = isVert ? trackRect.top : trackRect.left;
      dragStateRef.current = { pointerOffset, trackStart };
      setDragging(true);
      prevUserSelectRef.current = document.body.style.userSelect;
      document.body.style.userSelect = "none";
      try {
        thumbEl.setPointerCapture(e.pointerId);
      } catch {
        // 忽略捕获失败
      }
    },
    [containerEl, orientation, thumbLen],
  );

  const onThumbPointerMove = React.useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      if (!dragging || !containerEl || !dragStateRef.current || thumbLen === 0) return;
      const isVert = orientation === "vertical";
      const { pointerOffset, trackStart } = dragStateRef.current;
      const client = isVert ? containerEl.clientHeight : containerEl.clientWidth;
      const scroll = isVert ? containerEl.scrollHeight : containerEl.scrollWidth;
      // 期望的 thumb 位置 = 指针位置 - 轨道起点 - 指针在 thumb 内的偏移
      const pointerPos = isVert ? e.clientY : e.clientX;
      const desiredThumbPos = pointerPos - trackStart - pointerOffset;
      const nextScroll = scrollOffsetFromThumb(desiredThumbPos, client, scroll, thumbLen);
      if (isVert) containerEl.scrollTop = nextScroll;
      else containerEl.scrollLeft = nextScroll;
    },
    [dragging, containerEl, orientation, thumbLen],
  );

  const endDrag = React.useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      if (!dragging) return;
      setDragging(false);
      dragStateRef.current = null;
      document.body.style.userSelect = prevUserSelectRef.current;
      const thumbEl = thumbRef.current;
      if (thumbEl) {
        try {
          thumbEl.releasePointerCapture(e.pointerId);
        } catch {
          // 忽略
        }
      }
    },
    [dragging],
  );

  // 无需滚动时不渲染
  if (thumbLen === 0) return null;

  const visible = hovered || scrolling || dragging;

  const isVert = orientation === "vertical";

  return (
    <div
      ref={trackRef}
      onPointerEnter={onTrackEnter}
      onPointerLeave={onTrackLeave}
      className={cn(
        "absolute z-10 transition-opacity duration-200",
        isVert ? "bottom-0 right-0 top-0 w-[10px]" : "bottom-0 left-0 right-0 h-[10px]",
        visible ? "opacity-100" : "pointer-events-none opacity-0",
      )}
      aria-hidden="true"
    >
      <div
        ref={thumbRef}
        role="presentation"
        onPointerDown={onThumbPointerDown}
        onPointerMove={onThumbPointerMove}
        onPointerUp={endDrag}
        onPointerCancel={endDrag}
        className={cn(
          "absolute rounded-full bg-muted-foreground/35 transition-[width,height,background-color] duration-150",
          // 拖拽/悬停时更不透明且加宽
          hovered || dragging ? "bg-muted-foreground/55" : "bg-muted-foreground/35",
          isVert
            ? hovered || dragging
              ? "left-1/2 w-[7px] -translate-x-1/2"
              : "left-1/2 w-[5px] -translate-x-1/2"
            : hovered || dragging
              ? "top-1/2 h-[7px] -translate-y-1/2"
              : "top-1/2 h-[5px] -translate-y-1/2",
        )}
        style={
          isVert
            ? { height: thumbLen, top: thumbPos }
            : { width: thumbLen, left: thumbPos }
        }
      />
    </div>
  );
}

export type ScrollAreaProps = {
  className?: string;
  viewportClassName?: string;
  children: React.ReactNode;
  orientation?: "vertical" | "horizontal" | "both";
  /** 暴露 viewport（真实滚动元素）给调用方，用于程序化滚动（如 AI 消息列表自动滚到底） */
  viewportRef?: React.RefObject<HTMLDivElement | null>;
};

export function ScrollArea({ className, viewportClassName, children, orientation = "vertical", viewportRef }: ScrollAreaProps) {
  const innerRef = React.useRef<HTMLDivElement | null>(null);
  const [viewportEl, setViewportEl] = React.useState<HTMLDivElement | null>(null);

  React.useEffect(() => {
    setViewportEl(innerRef.current);
  }, []);

  // 合并内部 ref 与外部 viewportRef
  const setRefs = React.useCallback(
    (el: HTMLDivElement | null) => {
      innerRef.current = el;
      if (viewportRef) {
        (viewportRef as React.MutableRefObject<HTMLDivElement | null>).current = el;
      }
    },
    [viewportRef],
  );

  return (
    <div className={cn("relative overflow-hidden", className)}>
      <div
        ref={setRefs}
        className={cn(
          // 非渲染 thumb 的轴向上裁掉溢出，避免无 thumb 可拖的死滚动
          orientation === "vertical" && "overflow-y-scroll overflow-x-hidden",
          orientation === "horizontal" && "overflow-x-scroll overflow-y-hidden",
          orientation === "both" && "overflow-scroll",
          "no-native-scrollbar h-full w-full",
          viewportClassName,
        )}
      >
        {children}
      </div>
      {(orientation === "vertical" || orientation === "both") && (
        <ScrollbarTrack containerEl={viewportEl} orientation="vertical" />
      )}
      {(orientation === "horizontal" || orientation === "both") && (
        <ScrollbarTrack containerEl={viewportEl} orientation="horizontal" />
      )}
    </div>
  );
}

export type OverlayScrollbarProps = {
  containerRef: React.RefObject<HTMLElement | null>;
  orientation?: "vertical" | "horizontal";
};

export function OverlayScrollbar({ containerRef, orientation = "vertical" }: OverlayScrollbarProps) {
  const [el, setEl] = React.useState<HTMLElement | null>(() => containerRef.current);

  React.useEffect(() => {
    setEl(containerRef.current);
    // 容器可能是异步创建（如 CM 的 scrollDOM），用短轮询兜底
    const id = window.setInterval(() => {
      if (containerRef.current !== el) setEl(containerRef.current);
    }, 120);
    return () => window.clearInterval(id);
  }, [containerRef, el]);

  if (!el) return null;
  return <ScrollbarTrack containerEl={el} orientation={orientation} />;
}
