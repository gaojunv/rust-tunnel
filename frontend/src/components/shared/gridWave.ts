export interface GridCell {
  x: number;
  y: number;
}

/**
 * 衰减曲线：dist 在 [0, radius) 时返回 (0, 1]，dist >= radius 返回 0。
 * 用平方衰减让中心更聚、边缘快速归零，避免"探照灯"式的均匀衰减。
 */
export function computeIntensity(dist: number, radius: number): number {
  if (radius <= 0) return 0;
  if (dist >= radius) return 0;
  const t = 1 - dist / radius;
  return t * t;
}

/**
 * 行波：常驻背景律动。
 * 以正弦在水平方向推进，幅度随 y 接近中心带略强，返回 [0, 1]。
 */
export function ambientWave(x: number, y: number, time: number, height: number): number {
  // 主波：沿 x 推进的长波长正弦
  const primary = Math.sin(x * 0.025 - time * 1.6);
  // 次波：反向短波，叠加产生更丰富的律动
  const secondary = Math.sin(x * 0.06 + time * 2.3 + y * 0.018) * 0.4;
  // 中心带权重：靠近垂直中心略强，边缘淡出
  const centerY = height / 2;
  const verticalFalloff = Math.max(0, 1 - Math.abs(y - centerY) / (height * 0.7));
  // primary + secondary ∈ [-1.4, 1.4] → 归一化到 [0, 1]
  const v = (primary + secondary + 1.4) / 2.8;
  return v * (0.25 + 0.55 * verticalFalloff); // 整体压低，作背景律动
}

/**
 * 鼠标涟漪：以鼠标为中心向外扩散的环形波。
 * - ringPhase = dist - time * speed：随时间向外推进
 * - 仅在波前附近（带宽内）点亮，产生"圈"状的律动
 * - 整体乘以距离平方衰减 → 越远越暗，超过 radius 直接为 0
 */
export function rippleWave(
  dist: number,
  time: number,
  radius: number,
  waveSpeed: number,
  bandWidth: number,
): number {
  if (dist >= radius) return 0;
  const ringPhase = dist - time * waveSpeed;
  // 环形波前：仅在 ringPhase 接近 0 时点亮（窄带高斯）
  const ring = Math.exp(-(ringPhase * ringPhase) / (bandWidth * bandWidth));
  // 距离衰减（平方） → 越远越暗，满足"离标题越远高亮越小"
  const falloff = computeIntensity(dist, radius);
  return ring * falloff;
}

/**
 * 生成从 (0,0) 开始、步长 step 的网格坐标（左上角原点）。
 * 只包含完整落在 [0, width] x [0, height] 内的点。
 */
export function buildGrid(width: number, height: number, step: number): GridCell[] {
  if (width <= 0 || height <= 0 || step <= 0) return [];
  const cells: GridCell[] = [];
  for (let y = 0; y <= height; y += step) {
    for (let x = 0; x <= width; x += step) {
      cells.push({ x, y });
    }
  }
  return cells;
}
