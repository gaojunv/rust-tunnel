import { describe, expect, it } from 'vitest';
import { configStateModelValue, currentOptionLabel, normalizeConfigOptions } from './sessionConfig';

describe('normalizeConfigOptions', () => {
  it('flattens grouped select options and keeps currentValue', () => {
    const out = normalizeConfigOptions([
      {
        id: 'mode',
        name: 'Mode',
        category: 'mode',
        type: 'select',
        currentValue: 'plan',
        options: [
          { group: 'g1', name: 'G1', options: [{ value: 'default', name: 'Default' }] },
          { value: 'plan', name: 'Plan' },
        ],
      },
    ]);
    expect(out).toHaveLength(1);
    expect(out[0].currentValue).toBe('plan');
    expect(out[0].options?.map((o) => o.value)).toEqual(['default', 'plan']);
  });

  it('normalizes boolean options', () => {
    const out = normalizeConfigOptions([
      { id: 'fast', name: 'Fast', type: 'boolean', currentValue: true },
    ]);
    expect(out[0].type).toBe('boolean');
    expect(out[0].currentBool).toBe(true);
    expect(out[0].currentValue).toBe('true');
  });

  it('drops malformed entries and keeps unknown categories', () => {
    const out = normalizeConfigOptions([
      null,
      { name: 'no-id' },
      { id: 'x', name: 'X', type: 'select', currentValue: 'a', category: '_custom',
        options: [{ value: 'a', name: 'A' }] },
    ]);
    expect(out).toHaveLength(1);
    expect(out[0].category).toBe('_custom');
  });
});

describe('currentOptionLabel', () => {
  it('returns option name for current value, falls back to raw value', () => {
    expect(
      currentOptionLabel({
        id: 'mode', name: 'Mode', type: 'select', currentValue: 'plan',
        options: [{ value: 'plan', name: 'Plan' }],
      }),
    ).toBe('Plan');
    expect(
      currentOptionLabel({ id: 'mode', name: 'Mode', type: 'select', currentValue: 'x', options: [] }),
    ).toBe('x');
  });
});

describe('configStateModelValue', () => {
  it('extracts model value from persisted config_state', () => {
    expect(configStateModelValue('{"model":"opus","mode":"plan"}')).toBe('opus');
  });

  it('returns undefined for empty/missing/malformed input', () => {
    expect(configStateModelValue(undefined)).toBeUndefined();
    expect(configStateModelValue(null)).toBeUndefined();
    expect(configStateModelValue('')).toBeUndefined();
    expect(configStateModelValue('   ')).toBeUndefined();
    expect(configStateModelValue('not-json')).toBeUndefined();
    expect(configStateModelValue('["model"]')).toBeUndefined();
    expect(configStateModelValue('{"mode":"plan"}')).toBeUndefined();
    expect(configStateModelValue('{"model": 1}')).toBeUndefined();
    expect(configStateModelValue('{"model":"  "}')).toBeUndefined();
  });
});
