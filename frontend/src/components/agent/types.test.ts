import { describe, expect, it } from 'vitest';
import { parseAcpToolJson, parsePlanEntries } from './types';

describe('parseAcpToolJson', () => {
  it('parses kind/diffs/locations from normalized tool_calls json', () => {
    const json = JSON.stringify([{
      id: 'c1',
      name: 'Edit',
      arguments: '{"file_path":"a.ts"}',
      tool_kind: 'edit',
      diffs: [{ path: 'a.ts', old_text: 'x', new_text: 'y' }],
      locations: [{ path: 'a.ts', line: 3 }],
    }]);
    expect(parseAcpToolJson(json)).toEqual({
      toolKind: 'edit',
      toolDiffs: [{ path: 'a.ts', old_text: 'x', new_text: 'y' }],
      toolLocations: [{ path: 'a.ts', line: 3 }],
    });
  });

  it('tolerates missing fields and unknown kinds', () => {
    expect(parseAcpToolJson(JSON.stringify([{ id: 'c1', name: 'x' }]))).toEqual({});
    expect(parseAcpToolJson(JSON.stringify([{ tool_kind: 'teleport' }]))).toEqual({});
  });

  it('returns empty object on malformed json / old runner format', () => {
    expect(parseAcpToolJson('not json')).toEqual({});
    // runner 旧格式：function.arguments 嵌套，无 tool_kind → 不报错
    expect(
      parseAcpToolJson(JSON.stringify([{ id: 'c1', function: { name: 'shell', arguments: '{}' } }])),
    ).toEqual({});
  });
});

describe('parsePlanEntries', () => {
  it('parses entries and normalizes unknown status to pending', () => {
    const entries = parsePlanEntries(JSON.stringify([
      { content: 'a', status: 'completed' },
      { content: 'b', status: 'weird' },
    ]));
    expect(entries).toEqual([
      { content: 'a', status: 'completed' },
      { content: 'b', status: 'pending' },
    ]);
  });

  it('returns [] on malformed json', () => {
    expect(parsePlanEntries('oops')).toEqual([]);
  });
});
