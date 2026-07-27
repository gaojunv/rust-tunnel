// 文字 → 粒子点阵采样（纯函数，不依赖 React）。
// 离屏 canvas 绘制文字后读像素，按 step 步长采样 alpha 超过阈值的位置。

export interface TextParticle {
  homeX: number;
  homeY: number;
}

export interface SampleOptions {
  fontSizePx?: number;
  step?: number;
  dpr?: number;
  /** 粒子数上限：超出时按均匀间隔抽稀，防止中文长标题粒子过多卡顿。 */
  maxParticles?: number;
}

const ALPHA_THRESHOLD = 128;

// 均匀抽稀到不超过 max 个（保持整体字形分布）。
function thinOut(particles: TextParticle[], max: number): TextParticle[] {
  if (particles.length <= max) return particles;
  const stride = particles.length / max;
  const out: TextParticle[] = [];
  for (let i = 0; i < max; i++) {
    out.push(particles[Math.floor(i * stride)]);
  }
  return out;
}

export function sampleTextParticles(text: string, opts: SampleOptions = {}): TextParticle[] {
  const trimmed = text.trim();
  if (!trimmed) return [];

  const fontSizePx = opts.fontSizePx ?? 24;
  const step = opts.step ?? 3;
  const dpr = opts.dpr ?? 1;
  const font = `700 ${fontSizePx * dpr}px ui-sans-serif, system-ui, sans-serif`;

  const canvas = document.createElement('canvas');
  const ctx = canvas.getContext('2d', { willReadFrequently: true });
  if (!ctx) return [];

  ctx.font = font;
  const metrics = ctx.measureText(trimmed);
  const width = Math.max(1, Math.ceil(metrics.width));
  const height = Math.max(1, Math.ceil(fontSizePx * dpr * 1.4));
  canvas.width = width;
  canvas.height = height;

  // 设置 canvas 尺寸会重置绘图状态，需重新设置字体。
  ctx.font = font;
  ctx.textBaseline = 'middle';
  ctx.fillStyle = '#fff';
  ctx.fillText(trimmed, 0, height / 2);

  let imageData: ImageData;
  try {
    imageData = ctx.getImageData(0, 0, width, height);
  } catch {
    return [];
  }

  const particles: TextParticle[] = [];
  const sampleStep = Math.max(1, Math.round(step * dpr));
  for (let y = 0; y < height; y += sampleStep) {
    for (let x = 0; x < width; x += sampleStep) {
      const alpha = imageData.data[(y * width + x) * 4 + 3];
      if (alpha > ALPHA_THRESHOLD) {
        // 输出 CSS 像素坐标（除以 dpr），渲染时再乘回 dpr。
        particles.push({ homeX: x / dpr, homeY: y / dpr });
      }
    }
  }
  return thinOut(particles, opts.maxParticles ?? Infinity);
}
