import { describe, expect, it } from 'vitest';
import { parseContextLimit, mergeContextLimit } from './contextLimit';

describe('parseContextLimit', () => {
  it('returns 256k for missing or null extra_config', () => {
    expect(parseContextLimit()).toBe('256k');
    expect(parseContextLimit(null)).toBe('256k');
    expect(parseContextLimit(undefined)).toBe('256k');
  });

  it('returns 1m when agent_context_limit >= 4_194_304', () => {
    expect(parseContextLimit('{"agent_context_limit":4194304}')).toBe('1m');
    expect(parseContextLimit('{"agent_context_limit":5000000}')).toBe('1m');
    expect(parseContextLimit('{"agent_context_limit":10000000}')).toBe('1m');
  });

  it('returns 256k when agent_context_limit < 4_194_304', () => {
    expect(parseContextLimit('{"agent_context_limit":4194303}')).toBe('256k');
    expect(parseContextLimit('{"agent_context_limit":200000}')).toBe('256k');
    expect(parseContextLimit('{"agent_context_limit":0}')).toBe('256k');
    expect(parseContextLimit('{"agent_context_limit":100000}')).toBe('256k');
  });

  it('returns 256k for invalid JSON or non-numeric limit', () => {
    expect(parseContextLimit('not-json')).toBe('256k');
    expect(parseContextLimit('{"agent_context_limit":"4096"}')).toBe('256k');
    expect(parseContextLimit('{"agent_context_limit":true}')).toBe('256k');
    expect(parseContextLimit('{"agent_context_limit":null}')).toBe('256k');
    expect(parseContextLimit('{}')).toBe('256k');
  });

  it('ignores other keys in extra_config', () => {
    expect(parseContextLimit('{"compat_tool_history":true,"agent_context_limit":4194304}')).toBe('1m');
    expect(parseContextLimit('{"compat_tool_history":true}')).toBe('256k');
  });
});

describe('mergeContextLimit', () => {
  it('writes agent_context_limit=4194304 for 1m tier', () => {
    expect(mergeContextLimit(null, '1m')).toBe('{"agent_context_limit":4194304}');
    expect(mergeContextLimit(undefined, '1m')).toBe('{"agent_context_limit":4194304}');
    expect(mergeContextLimit('{"compat_tool_history":true}', '1m')).toBe(
      '{"compat_tool_history":true,"agent_context_limit":4194304}'
    );
  });

  it('deletes the key for 256k tier (default)', () => {
    expect(mergeContextLimit('{"agent_context_limit":4194304}', '256k')).toBe(null);
    expect(mergeContextLimit('{"agent_context_limit":4194304}', '256k')).toBe(null);
  });

  it('preserves other keys when removing limit for 256k', () => {
    expect(mergeContextLimit('{"compat_tool_history":true,"agent_context_limit":4194304}', '256k')).toBe(
      '{"compat_tool_history":true}'
    );
  });

  it('returns null when the merged object is empty', () => {
    expect(mergeContextLimit(null, '256k')).toBe(null);
    expect(mergeContextLimit('{}', '256k')).toBe(null);
  });

  it('tolerates invalid existing JSON', () => {
    expect(mergeContextLimit('not-json', '1m')).toBe('{"agent_context_limit":4194304}');
    expect(mergeContextLimit('not-json', '256k')).toBe(null);
  });
});
