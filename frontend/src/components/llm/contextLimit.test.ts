import { describe, expect, it } from 'vitest';
import { parseContextLimit, mergeContextLimit } from './contextLimit';

describe('parseContextLimit', () => {
  it('returns empty for missing or null extra_config', () => {
    expect(parseContextLimit()).toBe('');
    expect(parseContextLimit(null)).toBe('');
    expect(parseContextLimit(undefined)).toBe('');
  });

  it('reads the numeric agent_context_limit from extra_config JSON', () => {
    expect(parseContextLimit('{"agent_context_limit":4096}')).toBe('4096');
    expect(parseContextLimit('{"agent_context_limit":0}')).toBe('0');
    expect(parseContextLimit('{"compat_tool_history":true,"agent_context_limit":8192}')).toBe('8192');
  });

  it('returns empty for invalid JSON or non-numeric limit', () => {
    expect(parseContextLimit('not-json')).toBe('');
    expect(parseContextLimit('{"agent_context_limit":"4096"}')).toBe('');
    expect(parseContextLimit('{"agent_context_limit":true}')).toBe('');
    expect(parseContextLimit('{"agent_context_limit":null}')).toBe('');
    expect(parseContextLimit('{}')).toBe('');
  });
});

describe('mergeContextLimit', () => {
  it('adds agent_context_limit and keeps other keys', () => {
    expect(mergeContextLimit('{"compat_tool_history":true}', '4096')).toBe('{"compat_tool_history":true,"agent_context_limit":4096}');
    expect(mergeContextLimit(null, '4096')).toBe('{"agent_context_limit":4096}');
    expect(mergeContextLimit(undefined, '4096')).toBe('{"agent_context_limit":4096}');
  });

  it('floors fractional input and trims whitespace', () => {
    expect(mergeContextLimit(null, '4096.7')).toBe('{"agent_context_limit":4096}');
    expect(mergeContextLimit(null, '  4096  ')).toBe('{"agent_context_limit":4096}');
  });

  it('deletes the key for empty or invalid input', () => {
    expect(mergeContextLimit('{"agent_context_limit":4096}', '')).toBe(null);
    expect(mergeContextLimit('{"agent_context_limit":4096}', '  ')).toBe(null);
    expect(mergeContextLimit('{"agent_context_limit":4096}', 'abc')).toBe(null);
    expect(mergeContextLimit('{"agent_context_limit":4096}', '0')).toBe(null);
    expect(mergeContextLimit('{"agent_context_limit":4096}', '-5')).toBe(null);
  });

  it('preserves other keys when deleting the limit', () => {
    expect(mergeContextLimit('{"compat_tool_history":true,"agent_context_limit":4096}', '')).toBe('{"compat_tool_history":true}');
  });

  it('returns null when the merged object is empty', () => {
    expect(mergeContextLimit(null, '')).toBe(null);
    expect(mergeContextLimit('{}', '')).toBe(null);
  });

  it('tolerates invalid existing JSON', () => {
    expect(mergeContextLimit('not-json', '4096')).toBe('{"agent_context_limit":4096}');
    expect(mergeContextLimit('not-json', '')).toBe(null);
  });
});
