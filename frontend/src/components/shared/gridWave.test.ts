import { describe, expect, it } from 'vitest';
import { ambientWave, buildGrid, computeIntensity, rippleWave } from './gridWave';

describe('computeIntensity', () => {
  it('returns 1 at distance 0', () => {
    expect(computeIntensity(0, 140)).toBe(1);
  });

  it('returns 0 at distance >= radius', () => {
    expect(computeIntensity(140, 140)).toBe(0);
    expect(computeIntensity(200, 140)).toBe(0);
  });

  it('falls off quadratically between 0 and radius', () => {
    expect(computeIntensity(70, 140)).toBeCloseTo(0.25, 5);
    expect(computeIntensity(35, 140)).toBeCloseTo(0.5625, 5);
  });

  it('handles zero radius gracefully', () => {
    expect(computeIntensity(0, 0)).toBe(0);
  });
});

describe('ambientWave', () => {
  it('returns values in [0, 1]', () => {
    for (let x = 0; x < 200; x += 17) {
      for (let y = 0; y < 60; y += 13) {
        const v = ambientWave(x, y, 1.23, 60);
        expect(v).toBeGreaterThanOrEqual(0);
        expect(v).toBeLessThanOrEqual(1);
      }
    }
  });

  it('varies over time (律动)', () => {
    const a = ambientWave(50, 30, 0, 60);
    const b = ambientWave(50, 30, 1.5, 60);
    expect(a).not.toBeCloseTo(b, 5);
  });

  it('varies across x (波形在水平方向推进)', () => {
    const a = ambientWave(0, 30, 0, 60);
    const b = ambientWave(80, 30, 0, 60);
    expect(a).not.toBeCloseTo(b, 5);
  });
});

describe('rippleWave', () => {
  it('returns 0 beyond radius (远到直接无效果)', () => {
    expect(rippleWave(200, 1, 140, 220, 30)).toBe(0);
    expect(rippleWave(140, 1, 140, 220, 30)).toBe(0);
  });

  it('peaks on the wave front and decays with distance', () => {
    const time = 0.3;
    const speed = 220;
    const frontDist = time * speed; // 66
    const atFront = rippleWave(frontDist, time, 200, speed, 30);
    const offFront = rippleWave(frontDist + 60, time, 200, speed, 30);
    expect(atFront).toBeGreaterThan(offFront);
  });

  it('decays with distance from the cursor (离标题越远高亮越小)', () => {
    const near = rippleWave(40, 40 / 220, 200, 220, 30);
    const far = rippleWave(120, 120 / 220, 200, 220, 30);
    expect(near).toBeGreaterThan(far);
  });

  it('wave front advances with time (律动)', () => {
    const fixed = 80;
    const t1 = rippleWave(fixed, 0.1, 200, 220, 30);
    const t2 = rippleWave(fixed, 0.35, 200, 220, 30);
    expect(t1).not.toBeCloseTo(t2, 5);
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
    expect(cells).toHaveLength(2);
  });

  it('returns empty array for zero-size', () => {
    expect(buildGrid(0, 0, 14)).toEqual([]);
  });
});
