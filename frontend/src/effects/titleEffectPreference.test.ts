import { describe, expect, it } from 'vitest';
import {
  DEFAULT_TITLE_EFFECT,
  TITLE_EFFECT_PREFERENCES,
  isTitleEffectPreference,
} from './titleEffectPreference';

describe('titleEffectPreference', () => {
  it('default is grid-wave', () => {
    expect(DEFAULT_TITLE_EFFECT).toBe('grid-wave');
  });

  it('exposes all three modes', () => {
    expect(TITLE_EFFECT_PREFERENCES).toEqual(['particles', 'grid-wave', 'none']);
  });

  it.each(['particles', 'grid-wave', 'none'])('accepts %s', (value) => {
    expect(isTitleEffectPreference(value)).toBe(true);
  });

  it.each(['sparkle', '', 'PARTICLES', null, undefined, 42])('rejects %s', (value) => {
    expect(isTitleEffectPreference(value)).toBe(false);
  });
});
