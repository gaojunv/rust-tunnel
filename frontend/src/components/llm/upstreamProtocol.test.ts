import { describe, expect, it } from 'vitest';
import { parseUpstreamProtocol, mergeUpstreamProtocol } from './upstreamProtocol';

describe('parseUpstreamProtocol', () => {
  it('returns chat_completions for missing or null extra_config', () => {
    expect(parseUpstreamProtocol()).toBe('chat_completions');
    expect(parseUpstreamProtocol(null)).toBe('chat_completions');
    expect(parseUpstreamProtocol(undefined)).toBe('chat_completions');
  });

  it('returns responses when upstream_protocol is "responses"', () => {
    expect(parseUpstreamProtocol('{"upstream_protocol":"responses"}')).toBe('responses');
  });

  it('returns chat_completions for any other value', () => {
    expect(parseUpstreamProtocol('{"upstream_protocol":"chat_completions"}')).toBe('chat_completions');
    expect(parseUpstreamProtocol('{"upstream_protocol":"invalid"}')).toBe('chat_completions');
    expect(parseUpstreamProtocol('{"upstream_protocol":123}')).toBe('chat_completions');
    expect(parseUpstreamProtocol('{"upstream_protocol":true}')).toBe('chat_completions');
    expect(parseUpstreamProtocol('{"upstream_protocol":null}')).toBe('chat_completions');
  });

  it('returns chat_completions for invalid JSON', () => {
    expect(parseUpstreamProtocol('not-json')).toBe('chat_completions');
  });

  it('ignores other keys in extra_config', () => {
    expect(parseUpstreamProtocol('{"agent_context_limit":4194304,"upstream_protocol":"responses"}')).toBe('responses');
    expect(parseUpstreamProtocol('{"agent_context_limit":4194304}')).toBe('chat_completions');
  });

  it('returns chat_completions for empty object', () => {
    expect(parseUpstreamProtocol('{}')).toBe('chat_completions');
  });
});

describe('mergeUpstreamProtocol', () => {
  it('writes upstream_protocol for responses', () => {
    expect(mergeUpstreamProtocol(null, 'responses')).toBe('{"upstream_protocol":"responses"}');
    expect(mergeUpstreamProtocol(undefined, 'responses')).toBe('{"upstream_protocol":"responses"}');
  });

  it('deletes the key for chat_completions (default)', () => {
    expect(mergeUpstreamProtocol('{"upstream_protocol":"responses"}', 'chat_completions')).toBe(null);
  });

  it('preserves other keys when removing upstream_protocol', () => {
    expect(
      mergeUpstreamProtocol('{"agent_context_limit":4194304,"upstream_protocol":"responses"}', 'chat_completions'),
    ).toBe('{"agent_context_limit":4194304}');
  });

  it('preserves other keys when adding upstream_protocol', () => {
    expect(mergeUpstreamProtocol('{"agent_context_limit":4194304}', 'responses')).toBe(
      '{"agent_context_limit":4194304,"upstream_protocol":"responses"}',
    );
  });

  it('returns null when the merged object is empty', () => {
    expect(mergeUpstreamProtocol(null, 'chat_completions')).toBe(null);
    expect(mergeUpstreamProtocol('{}', 'chat_completions')).toBe(null);
  });

  it('tolerates invalid existing JSON', () => {
    expect(mergeUpstreamProtocol('not-json', 'responses')).toBe('{"upstream_protocol":"responses"}');
    expect(mergeUpstreamProtocol('not-json', 'chat_completions')).toBe(null);
  });
});
