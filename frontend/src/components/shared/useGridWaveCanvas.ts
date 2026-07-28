import { useEffect, useState } from 'react';
import { ambientWave, buildGrid, rippleWave } from './gridWave';
import { readPrimaryColor, type Hsl } from './particleColor';

const GRID_STEP = 12;          // 网格步长
const CELL_SIZE = 7;           // 方块边长
const RIPPLE_RADIUS = 220;     // 鼠标涟漪影响半径（超出直接无效果）
const RIPPLE_SPEED = 220;      // 波前扩散速度（px/s）
const RIPPLE_BAND = 32;        // 波前带宽（高斯 σ，决定圈的厚度）
const LERP = 0.18;             // 惯性插值系数
const MIN_ALPHA = 0.02;        // 低于此 alpha 跳过绘制
const AMBIENT_MAX_ALPHA = 0.18;// 背景律动最大透明度（压低，做底）
const RIPPLE_MAX_ALPHA = 0.85; // 涟漪峰值最大透明度
const MAX_RIPPLES = 6;         // 并发涟漪上限，快速连点时丢弃最早的
const RIPPLE_LIFETIME = RIPPLE_RADIUS / RIPPLE_SPEED + 0.5; // 单波寿命（秒），超过即回收

interface RuntimeCell {
  x: number;
  y: number;
  ambient: number; // 平滑后的背景律动强度
  ripple: number;  // 平滑后的鼠标涟漪强度
}

interface Ripple {
  x: number;
  y: number;
  startTime: number; // performance.now() / 1000
}

export function usePrefersReducedMotion(): boolean {
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

/**
 * 在 hostRef 元素上绘制网格波浪：常驻背景律动 + 点击时的扩散涟漪。
 * canvas 由调用方渲染（建议 absolute inset-0 放进 host 内），hook 负责：
 *  - 监听 host 尺寸并构建网格
 *  - 监听 host 上的 click 触发涟漪
 *  - requestAnimationFrame 持续绘制（含常驻背景律动）
 *
 * active: 是否启用。false 时直接早退，用于按 titleEffect 切换模式。
 */
export function useGridWaveCanvas(
  canvasRef: React.RefObject<HTMLCanvasElement | null>,
  hostRef: React.RefObject<HTMLElement | null>,
  active: boolean = true,
): void {
  const reducedMotion = usePrefersReducedMotion();

  useEffect(() => {
    if (!active || reducedMotion) return;
    const canvas = canvasRef.current;
    const host = hostRef.current;
    if (!canvas || !host) return;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;

    // primary 颜色可变：主题切换时 --primary CSS 变量改变，需要重新读取。
    // 用 MutationObserver 监听 <html> 的 class 变化（dark class 切换），
    // 比把 resolvedTheme 加入 effect 依赖更精准——不触发整个 effect 重建。
    let primary: Hsl | null = readPrimaryColor();
    if (!primary) return;

    const themeObserver = new MutationObserver(() => {
      const next = readPrimaryColor();
      if (next) primary = next;
    });
    themeObserver.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['class'],
    });

    let cells: RuntimeCell[] = [];
    let width = 0;
    let height = 0;

    const rebuildGrid = () => {
      const rect = host.getBoundingClientRect();
      width = Math.ceil(rect.width);
      height = Math.ceil(rect.height);
      canvas.width = width;
      canvas.height = height;

      const grid = buildGrid(width, height, GRID_STEP);
      cells = grid.map((c) => ({ x: c.x, y: c.y, ambient: 0, ripple: 0 }));
    };

    rebuildGrid();

    // 活跃涟漪队列（FIFO，新点击 push，超过寿命 shift 回收，超过上限丢弃最早的）。
    // 每个涟漪独立维护自己的扩散时钟，互不影响 → 连续点击会叠加多个扩散环。
    const ripples: Ripple[] = [];

    const onClick = (e: MouseEvent) => {
      const rect = canvas.getBoundingClientRect();
      ripples.push({
        x: e.clientX - rect.left,
        y: e.clientY - rect.top,
        startTime: performance.now() / 1000,
      });
      while (ripples.length > MAX_RIPPLES) ripples.shift();
    };
    host.addEventListener('click', onClick);

    let rafId = 0;
    const startTime = performance.now() / 1000;
    const draw = () => {
      const now = performance.now() / 1000;
      const time = now - startTime;

      // 回收寿命结束的涟漪
      while (ripples.length > 0 && now - ripples[0].startTime > RIPPLE_LIFETIME) {
        ripples.shift();
      }

      ctx.clearRect(0, 0, width, height);
      for (const cell of cells) {
        // 背景律动：常驻的行波，保证"不点击也有波浪感"
        const ambientTarget = ambientWave(cell.x, cell.y, time, height);
        cell.ambient += (ambientTarget - cell.ambient) * LERP;

        // 点击涟漪：取所有活跃涟漪的最大亮度（多波叠加不互相覆盖）
        let rippleTarget = 0;
        for (let i = 0; i < ripples.length; i++) {
          const r = ripples[i];
          const dx = cell.x - r.x;
          const dy = cell.y - r.y;
          const dist = Math.sqrt(dx * dx + dy * dy);
          const v = rippleWave(dist, now - r.startTime, RIPPLE_RADIUS, RIPPLE_SPEED, RIPPLE_BAND);
          if (v > rippleTarget) rippleTarget = v;
        }
        cell.ripple += (rippleTarget - cell.ripple) * LERP;

        const alpha = cell.ambient * AMBIENT_MAX_ALPHA + cell.ripple * RIPPLE_MAX_ALPHA;
        if (alpha > MIN_ALPHA && primary) {
          const size = CELL_SIZE * (1 + cell.ripple * 0.6);
          ctx.fillStyle = `hsl(${primary.h} ${primary.s}% ${primary.l}% / ${Math.min(1, alpha)})`;
          ctx.fillRect(cell.x - size / 2, cell.y - size / 2, size, size);
        }
      }
      rafId = requestAnimationFrame(draw);
    };
    rafId = requestAnimationFrame(draw);

    const ro = new ResizeObserver(() => {
      rebuildGrid();
    });
    ro.observe(host);

    return () => {
      ro.disconnect();
      themeObserver.disconnect();
      cancelAnimationFrame(rafId);
      host.removeEventListener('click', onClick);
    };
  }, [active, reducedMotion, canvasRef, hostRef]);
}
