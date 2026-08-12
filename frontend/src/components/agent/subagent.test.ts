import { describe, expect, it } from 'vitest';
import type { ChatItem } from './types';
import {
  appendChildStream,
  chunkKey,
  extractSubagentMeta,
  groupByParent,
  parseChunkKey,
  patchChildToolResult,
  upsertToolCard,
} from './subagent';

const tool = (overrides: Partial<ChatItem> & { toolId: string }): ChatItem => ({
  kind: 'tool',
  content: '',
  ...overrides,
});
const text = (content: string, parentToolId?: string): ChatItem => ({
  kind: 'assistant',
  content,
  parentToolId,
});

describe('groupByParent', () => {
  it('nests children into the parent tool card and keeps main items flat', () => {
    const flat: ChatItem[] = [
      { kind: 'user', content: '调研一下' },
      tool({ toolId: 'task1', toolName: 'Task', isSubagent: true, toolArgs: '{}' }),
      text('子代理思考', 'task1'),
      tool({ toolId: 'c1', toolName: 'Read x', toolKind: 'read', parentToolId: 'task1' }),
      text('主 agent 正文'),
    ];
    const grouped = groupByParent(flat);
    expect(grouped).toHaveLength(3); // user + task1(带 children) + 主正文
    expect(grouped[0]).toMatchObject({ kind: 'user' });
    expect(grouped[2]).toMatchObject({ kind: 'assistant', content: '主 agent 正文' });
    const parent = grouped[1];
    expect(parent.toolId).toBe('task1');
    expect(parent.children).toHaveLength(2);
    expect(parent.children![0]).toMatchObject({ kind: 'assistant', content: '子代理思考' });
    expect(parent.children![1]).toMatchObject({ kind: 'tool', toolId: 'c1', parentToolId: 'task1' });
  });

  it('supports arbitrary nesting depth (subagent of subagent)', () => {
    const flat: ChatItem[] = [
      tool({ toolId: 'task1', toolName: 'Task', isSubagent: true }),
      tool({ toolId: 'task2', toolName: 'Task', isSubagent: true, parentToolId: 'task1' }),
      tool({ toolId: 'c1', toolName: 'Read x', parentToolId: 'task2' }),
      text('最深层的文本', 'task2'),
    ];
    const grouped = groupByParent(flat);
    const t1 = grouped[0];
    const t2 = t1.children![0];
    expect(t2.toolId).toBe('task2');
    expect(t2.children!.map((c) => c.toolId ?? c.content)).toEqual(['c1', '最深层的文本']);
  });

  it('degrades orphan children (parent never appears) to top-level, content not lost', () => {
    const flat: ChatItem[] = [
      text('孤儿文本', 'ghost'),
      tool({ toolId: 'c1', toolName: 'Read x', parentToolId: 'ghost' }),
      { kind: 'user', content: '正常消息' },
    ];
    const grouped = groupByParent(flat);
    // 孤儿两项按到达顺序平铺回主流末尾（与 live 路径孤儿降级一致），内容不丢
    expect(grouped.map((g) => (g.kind === 'tool' ? g.toolId : g.content))).toEqual([
      '正常消息',
      '孤儿文本',
      'c1',
    ]);
  });

  it('attaches children even when the parent card arrives after its children', () => {
    const flat: ChatItem[] = [
      text('先到的子文本', 'task1'),
      tool({ toolId: 'c1', toolName: 'Read x', parentToolId: 'task1' }),
      tool({ toolId: 'task1', toolName: 'Task', isSubagent: true }),
    ];
    const grouped = groupByParent(flat);
    expect(grouped).toHaveLength(1);
    expect(grouped[0].toolId).toBe('task1');
    expect(grouped[0].children!.map((c) => (c.kind === 'tool' ? c.toolId : c.content))).toEqual([
      '先到的子文本',
      'c1',
    ]);
  });

  it('separates parallel subagents into independent lanes', () => {
    const flat: ChatItem[] = [
      tool({ toolId: 'taskA', toolName: 'Task', isSubagent: true }),
      tool({ toolId: 'taskB', toolName: 'Task', isSubagent: true }),
      tool({ toolId: 'a1', toolName: 'Read a', parentToolId: 'taskA' }),
      tool({ toolId: 'b1', toolName: 'Read b', parentToolId: 'taskB' }),
      text('A 的文本', 'taskA'),
      text('B 的文本', 'taskB'),
    ];
    const grouped = groupByParent(flat);
    const a = grouped[0];
    const b = grouped[1];
    expect(a.children!.map((c) => c.toolId ?? c.content)).toEqual(['a1', 'A 的文本']);
    expect(b.children!.map((c) => c.toolId ?? c.content)).toEqual(['b1', 'B 的文本']);
  });

  it('leaves items without parent linkage untouched (degradation path for non-ACP engines)', () => {
    const flat: ChatItem[] = [
      { kind: 'user', content: 'hi' },
      tool({ toolId: 'c1', toolName: 'Read x' }),
      text('回复'),
    ];
    expect(groupByParent(flat)).toEqual(flat);
  });
});

describe('extractSubagentMeta', () => {
  it('extracts description + subagent_type from Task args', () => {
    const meta = extractSubagentMeta(
      JSON.stringify({ description: '调研登录 bug', subagent_type: 'general-purpose', prompt: '...' }),
      'Task',
    );
    expect(meta).toEqual({
      label: '调研登录 bug',
      description: '调研登录 bug',
      subagentType: 'general-purpose',
    });
  });

  it('falls back to toolName when args are missing/empty/noop', () => {
    expect(extractSubagentMeta('{}', 'Agent')).toEqual({ label: 'Agent' });
    expect(extractSubagentMeta(undefined, 'Task')).toEqual({ label: 'Task' });
  });

  it('survives malformed args JSON', () => {
    const meta = extractSubagentMeta('not-json{{{', 'Task');
    expect(meta).toEqual({ label: 'Task' });
  });

  it('returns empty meta when nothing is available', () => {
    expect(extractSubagentMeta(undefined, undefined)).toEqual({});
  });
});

describe('upsertToolCard', () => {
  it('dedups by toolId, merging fields instead of duplicating', () => {
    const list = [tool({ toolId: 'c1', toolName: 'Read x', toolStatus: 'in_progress' })];
    const next = upsertToolCard(list, tool({ toolId: 'c1', toolName: 'Read y', toolStatus: 'running' }));
    expect(next).toHaveLength(1);
    expect(next[0].toolName).toBe('Read y');
    expect(next[0].toolStatus).toBe('running');
  });

  it('does not downgrade a completed card (late re-send ignored)', () => {
    const list = [tool({ toolId: 'c1', toolName: 'Read x', toolResult: 'ok', toolStatus: 'completed' })];
    const next = upsertToolCard(list, tool({ toolId: 'c1', toolName: 'Read x', toolStatus: 'in_progress' }));
    expect(next).toHaveLength(1);
    expect(next[0].toolResult).toBe('ok');
    expect(next[0].toolStatus).toBe('completed');
  });

  it('preserves existing children and appends late pending children on upgrade', () => {
    const list = [
      tool({ toolId: 'task1', toolName: 'Task', isSubagent: true, children: [text('已有子文本', 'task1')] }),
    ];
    const next = upsertToolCard(list, tool({ toolId: 'task1', toolName: 'Task', children: [text('迟到子文本', 'task1')] }));
    expect(next[0].children).toHaveLength(2);
    expect(next[0].children!.map((c) => c.content)).toEqual(['已有子文本', '迟到子文本']);
  });

  it('does not replace non-noop args with empty placeholders', () => {
    const list = [tool({ toolId: 'c1', toolName: 'Bash', toolArgs: '{"cmd":"ls"}' })];
    const next = upsertToolCard(list, tool({ toolId: 'c1', toolName: 'Bash', toolArgs: '{}' }));
    expect(next[0].toolArgs).toBe('{"cmd":"ls"}');
  });
});

describe('appendChildStream', () => {
  it('appends to the same-kind streaming bubble in the parent children', () => {
    const state = [
      tool({ toolId: 'task1', toolName: 'Task', children: [text('a', 'task1')] }),
    ];
    const r1 = appendChildStream(state, 'task1', 'assistant', 'b', { idx: 0, kind: 'assistant' });
    expect(r1.state[0].children![0].content).toBe('ab');
    expect(r1.attached).toBe(true);
    expect(r1.stream).toEqual({ idx: 0, kind: 'assistant' });
  });

  it('creates a new bubble when kind differs', () => {
    const state = [tool({ toolId: 'task1', toolName: 'Task', children: [text('a', 'task1')] })];
    const r1 = appendChildStream(state, 'task1', 'thought', 't', { idx: 0, kind: 'assistant' });
    expect(r1.state[0].children!.map((c) => c.kind)).toEqual(['assistant', 'thought']);
    expect(r1.stream).toEqual({ idx: 1, kind: 'thought' });
  });

  it('returns attached=false when the parent card is missing (orphan timing)', () => {
    const r = appendChildStream([{ kind: 'user', content: 'hi' }], 'ghost', 'assistant', 'x', null);
    expect(r.attached).toBe(false);
    expect(r.state).toHaveLength(1);
  });
});

describe('patchChildToolResult', () => {
  it('patches the matching child tool card in place', () => {
    const children = [
      tool({ toolId: 'c1', toolName: 'Read x', toolStatus: 'in_progress', toolArgs: '{}' }),
    ];
    const next = patchChildToolResult(children, {
      id: 'c1',
      name: 'Read x',
      result: 'src/',
      args: '{"path":"src/"}',
    });
    expect(next).toHaveLength(1);
    expect(next[0].toolResult).toBe('src/');
    expect(next[0].toolStatus).toBe('completed');
    expect(next[0].toolArgs).toBe('{"path":"src/"}');
  });

  it('appends a result-only card when the call card is missing (out-of-order frame)', () => {
    const children: ChatItem[] = [];
    const next = patchChildToolResult(children, { id: 'c9', name: 'Read z', result: 'r', parentToolId: 'task1' });
    expect(next).toHaveLength(1);
    expect(next[0]).toMatchObject({ toolId: 'c9', toolResult: 'r', parentToolId: 'task1' });
  });

  it('does not clobber real args already on the card', () => {
    const children = [tool({ toolId: 'c1', toolName: 'Bash', toolArgs: '{"cmd":"ls"}' })];
    const next = patchChildToolResult(children, { id: 'c1', result: 'ok' });
    expect(next[0].toolArgs).toBe('{"cmd":"ls"}');
  });
});

describe('chunkKey', () => {
  it('keys main vs child chunks separately so interleaved streams do not merge', () => {
    expect(chunkKey(undefined, 'assistant')).toBe(chunkKey(undefined, 'assistant'));
    expect(chunkKey('task1', 'assistant')).not.toBe(chunkKey(undefined, 'assistant'));
    expect(chunkKey('task1', 'thought')).not.toBe(chunkKey('task1', 'assistant'));
    expect(parseChunkKey(chunkKey('task1', 'thought'))).toEqual({ parent: 'task1', kind: 'thought' });
    expect(parseChunkKey(chunkKey(undefined, 'assistant'))).toEqual({ parent: '', kind: 'assistant' });
  });
});
