export interface GridCell {
  x: number;
  y: number;
}

/**
 * 中心亮、外围暗的线性衰减。
 * dist 在 [0, radius) 时返回 [1, 0)；dist >= radius 返回 0。
 */
export function computeIntensity(dist: number, radius: number): number {
  if (radius <= 0) return 0;
  if (dist >= radius) return 0;
  return 1 - dist / radius;
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
