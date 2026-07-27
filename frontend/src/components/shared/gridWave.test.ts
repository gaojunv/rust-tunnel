import { describe, expect, it } from 'vitest';
import { buildGrid, computeIntensity } from './gridWave';

describe('computeIntensity', () => {
  it('returns 1 at distance 0', () => {
    expect(computeIntensity(0, 140)).toBe(1);
  });

  it('returns 0 at distance >= radius', () => {
    expect(computeIntensity(140, 140)).toBe(0);
    expect(computeIntensity(200, 140)).toBe(0);
  });

  it('returns linear falloff between 0 and radius', () => {
    expect(computeIntensity(70, 140)).toBeCloseTo(0.5, 5);
    expect(computeIntensity(35, 140)).toBeCloseTo(0.75, 5);
  });

  it('handles zero radius gracefully', () => {
    expect(computeIntensity(0, 0)).toBe(0);
  });
});

describe('buildGrid', () => {
  it('produces a regular grid with the given step', () => {
    const cells = buildGrid(28, 28, 14);
    expect(cells).toHaveLength(9); // 3x3
    expect(cells[0]).toEqual({ x: 0, y: 0 });
    expect(cells[1]).toEqual({ x: 14, y: 0 });
    expect(cells[4]).toEqual({ x: 14, y: 14 });
  });

  it('handles partial cells by including them', () => {
    const cells = buildGrid(20, 10, 14);
    // x: 0, 14 (next would be 28 > 20); y: 0 only (next 14 > 10)
    expect(cells).toHaveLength(2);
  });

  it('returns empty array for zero-size', () => {
    expect(buildGrid(0, 0, 14)).toEqual([]);
  });
});
