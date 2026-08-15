import { describe, expect, it } from 'vitest';
import { parseAcpToolJson, parsePlanEntries, parseToolResultContent } from './types';

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

  it('tolerates null diffs/locations as backend persists them', () => {
    expect(parseAcpToolJson(JSON.stringify([{ id: 'c1', name: 'Edit', diffs: null, locations: null }]))).toEqual({});
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

describe('parseToolResultContent', () => {
  it('parses the new JSON contract: text + status + diffs + locations', () => {
    const raw = JSON.stringify({
      text: 'ok',
      status: 'failed',
      diffs: [{ path: 'a.ts', old_text: 'x', new_text: 'y' }],
      locations: [{ path: 'a.ts', line: 3 }],
    });
    expect(parseToolResultContent(raw)).toEqual({
      text: 'ok',
      status: 'failed',
      diffs: [{ path: 'a.ts', old_text: 'x', new_text: 'y' }],
      locations: [{ path: 'a.ts', line: 3 }],
    });
  });

  it('maps known statuses and ignores unknown ones', () => {
    expect(parseToolResultContent(JSON.stringify({ text: 'a', status: 'completed' })).status).toBe('completed');
    expect(parseToolResultContent(JSON.stringify({ text: 'b', status: 'running' })).status).toBe('running');
    expect(parseToolResultContent(JSON.stringify({ text: 'c', status: 'weird' })).status).toBeUndefined();
  });

  it('falls back to plain text for legacy rows (non-JSON or non-conforming JSON)', () => {
    expect(parseToolResultContent('fn main(){}')).toEqual({ text: 'fn main(){}' });
    expect(parseToolResultContent('')).toEqual({ text: '' });
    expect(parseToolResultContent('not json{{{')).toEqual({ text: 'not json{{{' });
    // JSON 但 text 非 string（数组/标量/缺字段）→ 不认新契约，按纯文本原样返回
    expect(parseToolResultContent('[1,2]')).toEqual({ text: '[1,2]' });
    expect(parseToolResultContent('{"status":"failed"}')).toEqual({ text: '{"status":"failed"}' });
    expect(parseToolResultContent('{"text":123}')).toEqual({ text: '{"text":123}' });
  });
});
