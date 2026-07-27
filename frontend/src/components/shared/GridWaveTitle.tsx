import { useEffect, useRef, useState } from 'react';
import { cn } from '../../lib/utils';
import { buildGrid, computeIntensity } from './gridWave';
import { readPrimaryColor } from './particleColor';

const GRID_STEP = 14;
const CELL_SIZE = 10;
const EXPAND = 120;           // canvas 在标题周围外扩的像素
const INFLUENCE_RADIUS = 140; // 鼠标影响半径
const LERP = 0.18;            // 惯性插值系数
const MIN_ALPHA = 0.01;       // 低于此 alpha 跳过绘制
const MAX_ALPHA = 0.55;       // 最亮方块的 alpha

export interface GridWaveTitleProps {
  text: string;
  className?: string;
  eventTargetRef?: React.RefObject<HTMLElement | null>;
}

function usePrefersReducedMotion(): boolean {
  const [reduced, setReduced] = useState(
    () =>
      typeof window !== 'undefined' &&
      window.matchMedia('(prefers-reduced-motion: reduce)').matches,
  );
  useEffect(() => {
    const mq = window.matchMedia('(prefers-reduced-motion: reduce)');
    const onChange = () => setReduced(mq.matches);
    mq.addEventListener?.('change', onChange);
    return () => mq.removeEventListener?.('change', onChange);
  }, []);
  return reduced;
}

interface RuntimeCell {
  x: number;
  y: number;
  intensity: number;
}

export function GridWaveTitle({ text, className, eventTargetRef }: GridWaveTitleProps) {
  const reducedMotion = usePrefersReducedMotion();
  const containerRef = useRef<HTMLSpanElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const textRef = useRef<HTMLSpanElement>(null);

  useEffect(() => {
    if (reducedMotion) return;
    const canvas = canvasRef.current;
    const textEl = textRef.current;
    const container = containerRef.current;
    if (!canvas || !textEl || !container) return;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;

    const primary = readPrimaryColor();
    if (!primary) return;

    let cells: RuntimeCell[] = [];
    let width = 0;
    let height = 0;

    const rebuildGrid = () => {
      const textRect = textEl.getBoundingClientRect();
      width = Math.ceil(textRect.width) + EXPAND * 2;
      height = Math.ceil(textRect.height) + EXPAND * 2;
      canvas.width = width;
      canvas.height = height;
      canvas.style.width = `${width}px`;
      canvas.style.height = `${height}px`;
      // 定位：canvas 中心对齐文字
      canvas.style.position = 'absolute';
      canvas.style.left = '50%';
      canvas.style.top = '50%';
      canvas.style.transform = 'translate(-50%, -50%)';
      canvas.style.pointerEvents = 'none';
      canvas.style.zIndex = '0';

      // 构建网格
      const grid = buildGrid(width, height, GRID_STEP);
      cells = grid.map((c) => ({ x: c.x, y: c.y, intensity: 0 }));
    };

    rebuildGrid();

    // 鼠标位置（相对 canvas）
    let mouseX = -Infinity;
    let mouseY = -Infinity;

    const eventTarget = eventTargetRef?.current ?? container;
    const onPointerMove = (e: PointerEvent) => {
      const rect = canvas.getBoundingClientRect();
      mouseX = e.clientX - rect.left;
      mouseY = e.clientY - rect.top;
    };
    const onPointerLeave = () => {
      mouseX = -Infinity;
      mouseY = -Infinity;
    };
    eventTarget.addEventListener('pointermove', onPointerMove);
    eventTarget.addEventListener('pointerleave', onPointerLeave);

    // 动画循环
    let rafId = 0;
    const draw = () => {
      ctx.clearRect(0, 0, width, height);
      for (const cell of cells) {
        const dx = cell.x - mouseX;
        const dy = cell.y - mouseY;
        const dist = Math.hypot(dx, dy);
        const target = computeIntensity(dist, INFLUENCE_RADIUS);
        cell.intensity += (target - cell.intensity) * LERP;
        if (cell.intensity > MIN_ALPHA) {
          ctx.fillStyle = `hsl(${primary.h} ${primary.s}% ${primary.l}% / ${cell.intensity * MAX_ALPHA})`;
          ctx.fillRect(
            cell.x - CELL_SIZE / 2,
            cell.y - CELL_SIZE / 2,
            CELL_SIZE,
            CELL_SIZE,
          );
        }
      }
      rafId = requestAnimationFrame(draw);
    };
    rafId = requestAnimationFrame(draw);

    // 监听容器 resize，自动重建网格
    const ro = new ResizeObserver(() => {
      rebuildGrid();
    });
    ro.observe(container);

    return () => {
      ro.disconnect();
      cancelAnimationFrame(rafId);
      eventTarget.removeEventListener('pointermove', onPointerMove);
      eventTarget.removeEventListener('pointerleave', onPointerLeave);
    };
  }, [reducedMotion, eventTargetRef, text]);

  if (reducedMotion) {
    return <span className={cn('text-aurora', className)}>{text}</span>;
  }

  return (
    <span
      ref={containerRef}
      className={cn('relative inline-block align-middle', className)}
    >
      <canvas ref={canvasRef} aria-hidden="true" />
      <span ref={textRef} className="text-aurora relative z-10">
        {text}
      </span>
    </span>
  );
}
