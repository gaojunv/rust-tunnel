// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';
import { sampleTextParticles } from './particleText';

describe('sampleTextParticles', () => {
  it('returns an empty array for empty text', () => {
    expect(sampleTextParticles('')).toEqual([]);
    expect(sampleTextParticles('   ')).toEqual([]);
  });

  it('produces particles within the text bounding box', () => {
    const fontSizePx = 24;
    const step = 3;
    const particles = sampleTextParticles('Dashboard', { fontSizePx, step, dpr: 1 });
    expect(particles.length).toBeGreaterThan(0);
    for (const p of particles) {
      expect(p.homeX).toBeGreaterThanOrEqual(0);
      expect(p.homeY).toBeGreaterThanOrEqual(0);
      // 不应超出合理范围（宽度上限取字号 * 文本长度的 1.2 倍，高度上限取字号的 1.5 倍）
      expect(p.homeX).toBeLessThanOrEqual(fontSizePx * 'Dashboard'.length * 1.2);
      expect(p.homeY).toBeLessThanOrEqual(fontSizePx * 1.5);
    }
  });

  it('produces more particles for longer text', () => {
    const short = sampleTextParticles('Logs', { fontSizePx: 24, step: 3 });
    const long = sampleTextParticles('Reverse Proxy Rules', { fontSizePx: 24, step: 3 });
    expect(long.length).toBeGreaterThan(short.length);
  });

  it('every particle has finite home coordinates', () => {
    const particles = sampleTextParticles('Clients');
    for (const p of particles) {
      expect(Number.isFinite(p.homeX)).toBe(true);
      expect(Number.isFinite(p.homeY)).toBe(true);
    }
  });
});
