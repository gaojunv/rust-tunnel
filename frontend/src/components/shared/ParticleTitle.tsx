import { useEffect, useRef, useState } from 'react';
import { cn } from '@/lib/utils';
import { sampleTextParticles } from './particleText';
import { readPrimaryColor } from './particleColor';

interface ParticleTitleProps {
  text: string;
  className?: string;
}

interface RuntimeParticle {
  homeX: number;
  homeY: number;
  x: number;
  y: number;
  vx: number;
  vy: number;
}

const FONT_SIZE = 24;
const STEP = 3;
const PARTICLE_SIZE = 2; // 小正方形边长（CSS px）
const LIGHT_BAND = 46; // 光带半径（px）
const REPEL_RADIUS = 56; // 扰动半径（px）
const REPEL_FORCE = 2.4; // 扰动推力
const SPRING = 0.06; // 归位弹簧系数
const DAMPING = 0.86; // 阻尼

function usePrefersReducedMotion(): boolean {
  const [reduced, setReduced] = useState(
    () => typeof window !== 'undefined' && window.matchMedia('(prefers-reduced-motion: reduce)').matches
  );
  useEffect(() => {
    const mq = window.matchMedia('(prefers-reduced-motion: reduce)');
    const onChange = () => setReduced(mq.matches);
    mq.addEventListener?.('change', onChange);
    return () => mq.removeEventListener?.('change', onChange);
  }, []);
  return reduced;
}

// Canvas 像素粒子标题：小方块颗粒拼字，悬停光波点亮 + 颗粒扰动散开，
// 默认轻微呼吸微光。遵循 prefers-reduced-motion（退化为静态颗粒）。
// canvas/主题色不可用时回退为普通渐变文字。
export function ParticleTitle({ text, className }: ParticleTitleProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [usable, setUsable] = useState<boolean | null>(null); // null=未知, true=粒子, false=回退
  const reducedMotion = usePrefersReducedMotion();

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    const hsl = readPrimaryColor();
    if (!ctx || !hsl) {
      setUsable(false);
      return;
    }

    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const sampled = sampleTextParticles(text, { fontSizePx: FONT_SIZE, step: STEP, dpr });
    if (sampled.length === 0) {
      setUsable(false);
      return;
    }

    // 由粒子范围确定 canvas 尺寸。
    const maxX = Math.max(...sampled.map((p) => p.homeX));
    const maxY = Math.max(...sampled.map((p) => p.homeY));
    const cssW = Math.ceil(maxX + STEP);
    const cssH = Math.ceil(maxY + STEP);
    canvas.width = cssW * dpr;
    canvas.height = cssH * dpr;
    canvas.style.width = `${cssW}px`;
    canvas.style.height = `${cssH}px`;

    const particles: RuntimeParticle[] = sampled.map((p) => ({
      homeX: p.homeX,
      homeY: p.homeY,
      x: p.homeX,
      y: p.homeY,
      vx: 0,
      vy: 0,
    }));

    const mouse = { x: -9999, y: -9999, active: false };
    let raf = 0;
    let t = 0;

    const draw = () => {
      t += 1;
      ctx.clearRect(0, 0, canvas.width, canvas.height);
      // 呼吸微光：整体亮度随时间正弦缓慢变化。
      const breathe = 0.5 + 0.5 * Math.sin(t * 0.02);
      // 自动漂移光带（不悬停时也缓慢移动）。
      const autoBandX = ((t * 0.6) % (cssW + 240)) - 120;

      for (const p of particles) {
        if (mouse.active) {
          const dx = p.x - mouse.x;
          const dy = p.y - mouse.y;
          const dist = Math.hypot(dx, dy);
          if (dist < REPEL_RADIUS && dist > 0.01) {
            const force = ((REPEL_RADIUS - dist) / REPEL_RADIUS) * REPEL_FORCE;
            p.vx += (dx / dist) * force;
            p.vy += (dy / dist) * force;
          }
        }
        // 弹簧归位 + 阻尼
        p.vx = (p.vx + (p.homeX - p.x) * SPRING) * DAMPING;
        p.vy = (p.vy + (p.homeY - p.y) * SPRING) * DAMPING;
        p.x += p.vx;
        p.y += p.vy;

        // 光波点亮：鼠标光带 + 自动光带叠加。
        const bandX = mouse.active ? mouse.x : autoBandX;
        const glow = Math.max(0, 1 - Math.abs(p.x - bandX) / LIGHT_BAND);
        const lightness = Math.min(90, hsl.l + breathe * 6 + glow * 30);
        const alpha = 0.55 + breathe * 0.15 + glow * 0.3;
        const size = PARTICLE_SIZE * (1 + glow * 0.5);
        ctx.fillStyle = `hsl(${hsl.h} ${hsl.s}% ${lightness}% / ${Math.min(1, alpha)})`;
        ctx.fillRect(p.x * dpr - (size * dpr) / 2, p.y * dpr - (size * dpr) / 2, size * dpr, size * dpr);
      }

      if (!reducedMotion) {
        raf = requestAnimationFrame(draw);
      }
    };

    draw(); // reduced-motion 下只画这一帧

    const onMove = (e: PointerEvent) => {
      const rect = canvas.getBoundingClientRect();
      mouse.x = e.clientX - rect.left;
      mouse.y = e.clientY - rect.top;
      mouse.active = true;
    };
    const onLeave = () => {
      mouse.active = false;
    };
    canvas.addEventListener('pointermove', onMove);
    canvas.addEventListener('pointerleave', onLeave);

    return () => {
      cancelAnimationFrame(raf);
      canvas.removeEventListener('pointermove', onMove);
      canvas.removeEventListener('pointerleave', onLeave);
    };
  }, [text, reducedMotion]);

  // 初次渲染时先挂 canvas（可用性未知也挂，由 effect 判定）。
  if (usable === false) {
    return <span className={cn('text-gradient', className)}>{text}</span>;
  }

  return (
    <span className={cn('relative inline-block align-middle', className)}>
      <canvas ref={canvasRef} role="img" aria-label={text} className="block" />
      <span className="sr-only">{text}</span>
    </span>
  );
}
