// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';
import { readPrimaryColor } from './particleColor';

const setPrimary = (v: string) =>
  document.documentElement.style.setProperty('--primary', v);

afterEach(() => {
  document.documentElement.style.removeProperty('--primary');
});

describe('readPrimaryColor', () => {
  it('parses an HSL triple from --primary', () => {
    setPrimary('221 83% 53%');
    expect(readPrimaryColor()).toEqual({ h: 221, s: 83, l: 53 });
  });

  it('handles extra whitespace', () => {
    setPrimary('  199   89%   55%  ');
    expect(readPrimaryColor()).toEqual({ h: 199, s: 89, l: 55 });
  });

  it('returns null when --primary is empty', () => {
    setPrimary('');
    expect(readPrimaryColor()).toBeNull();
  });

  it('returns null for malformed values', () => {
    setPrimary('not-a-color');
    expect(readPrimaryColor()).toBeNull();
  });
});
