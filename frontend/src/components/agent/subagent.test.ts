import { describe, expect, it } from 'vitest';
import type { ChatItem } from './types';
import {
  applyToolCallChunk,
  appendChildStream,
  chunkKey,
  collectSubagents,
  dropStreamPlaceholders,
  extractSubagentMeta,
  groupByParent,
  mergePages,
  parseChunkKey,
  patchChildToolResult,
  STREAM_TOOL_ID_PREFIX,
  subagentTypeMeta,
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

describe('mergePages', () => {
  it('re-groups cross-page orphans into the parent card from the earlier page', () => {
    // 分页边界：更早页含父 Task 卡，已加载页含其子项（父卡缺席 → 顶层孤儿平铺）。
    // mergePages 把孤儿收进父卡 children，顶层只剩父卡 + 无归属项。
    const older = [
      { kind: 'user' as const, content: '早期问题' },
      tool({ toolId: 'task1', toolName: 'Task', isSubagent: true, toolArgs: '{"description":"调研"}' }),
    ];
    const loaded = [
      text('子代理文本', 'task1'),
      tool({ toolId: 'c1', toolName: 'Read x', toolKind: 'read', parentToolId: 'task1', toolResult: 'ok' }),
      { kind: 'assistant' as const, content: '主回复' },
    ];
    const { items, absorbedIndexes } = mergePages(older, loaded);
    // 顶层 = user + 父卡 + 主回复（孤儿被收进父卡 children）
    expect(items.map((it) => (it.kind === 'tool' ? it.toolId : it.content))).toEqual([
      '早期问题',
      'task1',
      '主回复',
    ]);
    const parent = items[1] as ChatItem;
    expect(parent.children!.map((c) => (c.kind === 'tool' ? c.toolId : c.content))).toEqual([
      '子代理文本',
      'c1',
    ]);
    // 被吸收孤儿在 loaded 中的下标（用于流式 ref 位移修正）
    expect(absorbedIndexes).toEqual([0, 1]);
  });

  it('appends absorbed orphans after the parent children already in the earlier page', () => {
    const older = [
      tool({
        toolId: 'task1',
        toolName: 'Task',
        isSubagent: true,
        children: [tool({ toolId: 'c0', toolName: 'Read first', parentToolId: 'task1' })],
      }),
    ];
    const loaded = [text('迟到的子文本', 'task1')];
    const { items } = mergePages(older, loaded);
    const parent = items[0] as ChatItem;
    expect(parent.children!.map((c) => (c.kind === 'tool' ? c.toolId : c.content))).toEqual([
      'c0',
      '迟到的子文本',
    ]);
  });

  it('matches nested parents inside the earlier page (recursive attach)', () => {
    const older = [
      tool({
        toolId: 'task1',
        toolName: 'Task',
        isSubagent: true,
        children: [tool({ toolId: 'task2', toolName: 'Task', isSubagent: true, parentToolId: 'task1' })],
      }),
    ];
    const loaded = [text('最深层的文本', 'task2')];
    const { items, absorbedIndexes } = mergePages(older, loaded);
    const t1 = items[0] as ChatItem;
    const t2 = t1.children![0] as ChatItem;
    expect(t2.children!.map((c) => c.content)).toEqual(['最深层的文本']);
    expect(absorbedIndexes).toEqual([0]);
  });

  it('is a no-op when the earlier page has no matching parents or no orphans', () => {
    const older = [tool({ toolId: 'task1', toolName: 'Task', isSubagent: true })];
    const loaded = [{ kind: 'assistant' as const, content: '正文' }];
    const { items, absorbedIndexes } = mergePages(older, loaded);
    expect(items).toEqual([...older, ...loaded]);
    expect(absorbedIndexes).toEqual([]);
    // 孤儿指向不存在的父卡（更早页无该 id）→ 保持平铺，不丢内容
    const r2 = mergePages([{ kind: 'user' as const, content: 'hi' }], [text('孤儿', 'ghost')]);
    expect(r2.items).toHaveLength(2);
    expect(r2.absorbedIndexes).toEqual([]);
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

describe('subagentTypeMeta', () => {
  it('maps known types to localized labelKey + semantic chip colors', () => {
    expect(subagentTypeMeta('explore')).toEqual({
      labelKey: 'agent.subagentTypeExplore',
      chipClass: 'bg-teal-500/10',
      textClass: 'text-teal-600 dark:text-teal-400',
    });
    expect(subagentTypeMeta('general-purpose')).toEqual({
      labelKey: 'agent.subagentTypeGeneral',
      chipClass: 'bg-slate-500/10',
      textClass: 'text-slate-600 dark:text-slate-400',
    });
    expect(subagentTypeMeta('plan')).toEqual({
      labelKey: 'agent.subagentTypePlan',
      chipClass: 'bg-violet-500/10',
      textClass: 'text-violet-600 dark:text-violet-400',
    });
  });

  it('is case/whitespace tolerant for known types', () => {
    expect(subagentTypeMeta('  Explore ')).toEqual(subagentTypeMeta('explore'));
  });

  it('falls back to muted style with no labelKey for unknown types (raw value shown)', () => {
    const meta = subagentTypeMeta('custom-agent');
    expect(meta).toEqual({ chipClass: 'bg-muted', textClass: 'text-muted-foreground' });
    expect(meta!.labelKey).toBeUndefined();
  });

  it('returns undefined for empty/missing type (badge not rendered)', () => {
    expect(subagentTypeMeta(undefined)).toBeUndefined();
    expect(subagentTypeMeta('')).toBeUndefined();
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

describe('collectSubagents', () => {
  it('collects subagent parent cards with status/progress summary', () => {
    const items: ChatItem[] = [
      { kind: 'user', content: 'hi' },
      tool({
        toolId: 'task1',
        toolName: 'Task',
        isSubagent: true,
        toolArgs: '{"description":"A 任务","subagent_type":"general-purpose"}',
        toolStatus: 'in_progress',
        children: [
          tool({ toolId: 'c1', toolName: 'Read x', toolKind: 'read', toolStatus: 'completed', toolResult: 'ok' }),
          tool({ toolId: 'c2', toolName: 'Bash', toolKind: 'execute', toolStatus: 'running' }),
        ],
      }),
      text('主回复'),
    ];
    const out = collectSubagents(items);
    expect(out).toHaveLength(1);
    expect(out[0]).toMatchObject({
      index: 1,
      toolId: 'task1',
      label: 'A 任务',
      subagentType: 'general-purpose',
      status: 'in_progress',
      toolCount: 2,
    });
    // 当前运行工具 = 最后一个未完成子工具 → Bash 归一化 label Terminal
    expect(out[0].runningToolLabel).toBe('Terminal');
  });

  it('marks completed and failed parent cards with their status', () => {
    const items: ChatItem[] = [
      tool({ toolId: 'a', toolName: 'Task', isSubagent: true, toolStatus: 'completed', toolResult: 'done' }),
      tool({ toolId: 'b', toolName: 'Task', isSubagent: true, toolStatus: 'failed' }),
    ];
    const out = collectSubagents(items);
    expect(out.map((s) => s.status)).toEqual(['completed', 'failed']);
    expect(out[0].runningToolLabel).toBeNull();
  });

  it('ignores non-subagent tool cards and flat items', () => {
    const items: ChatItem[] = [
      { kind: 'user', content: 'hi' },
      tool({ toolId: 'c1', toolName: 'Read x', toolKind: 'read' }), // 顶层普通工具卡
    ];
    expect(collectSubagents(items)).toEqual([]);
  });

  it('includes tool cards with children even without is_subagent flag (history path)', () => {
    const items: ChatItem[] = [
      tool({
        toolId: 'task1',
        toolName: 'Task',
        children: [
          tool({ toolId: 'c1', toolName: 'Read x', parentToolId: 'task1', toolStatus: 'completed', toolResult: 'ok' }),
        ],
      }),
    ];
    const out = collectSubagents(items);
    expect(out).toHaveLength(1);
    expect(out[0].toolId).toBe('task1');
    expect(out[0].toolCount).toBe(1);
  });
});

describe('applyToolCallChunk', () => {
  it('routes chunk into parent card children when parent_tool_call_id is present', () => {
    const list: ChatItem[] = [
      tool({ toolId: 'task1', toolName: 'Task', isSubagent: true, children: [] }),
    ];
    const next = applyToolCallChunk(list, {
      parent_tool_call_id: 'task1',
      index: 0,
      name: 'read_file',
      arguments: '{"path":',
    });
    // Parent card still exists at top level
    expect(next).toHaveLength(1);
    expect(next[0].toolId).toBe('task1');
    // Child chunk routed into parent's children
    expect(next[0].children).toHaveLength(1);
    expect(next[0].children![0]).toMatchObject({
      kind: 'tool',
      toolName: 'read_file',
      toolArgs: '{"path":',
      toolStatus: 'in_progress',
    });
  });

  it('accumulates argument increments in the same child card', () => {
    const list: ChatItem[] = [
      tool({ toolId: 'task1', toolName: 'Task', isSubagent: true, children: [] }),
    ];
    const s1 = applyToolCallChunk(list, {
      parent_tool_call_id: 'task1',
      index: 0,
      name: 'read_file',
      arguments: '{"path":',
    });
    const s2 = applyToolCallChunk(s1, {
      parent_tool_call_id: 'task1',
      index: 0,
      arguments: '"a.rs"}',
    });
    expect(s2[0].children![0].toolArgs).toBe('{"path":"a.rs"}');
  });

  it('falls to main stream when parent_tool_call_id references a missing parent', () => {
    const list: ChatItem[] = [
      { kind: 'user', content: 'hi' },
    ];
    const next = applyToolCallChunk(list, {
      parent_tool_call_id: 'ghost',
      index: 0,
      name: 'read_file',
      arguments: '{"path":"x"}',
    });
    // Chunk lands in main stream as a standalone tool card (orphan, will be
    // grouped by groupByParent later when the parent card arrives)
    expect(next).toHaveLength(2);
    expect(next[1]).toMatchObject({
      kind: 'tool',
      toolName: 'read_file',
      toolArgs: '{"path":"x"}',
    });
  });

  it('does not mutate the original list (pure function)', () => {
    const list: ChatItem[] = [
      tool({ toolId: 'task1', toolName: 'Task', isSubagent: true, children: [] }),
    ];
    const next = applyToolCallChunk(list, {
      parent_tool_call_id: 'task1',
      index: 0,
      name: 'Bash',
      arguments: '{"cmd":"ls"}',
    });
    // Original list unchanged
    expect(list[0].children).toHaveLength(0);
    // New list has children
    expect(next[0].children).toHaveLength(1);
  });

  it('carries accumulated id/name from first chunk on subsequent chunks (standard OpenAI pattern)', () => {
    // 标准 OpenAI 流：首 chunk 带 id+name，后续仅 arguments。
    // 服务端修复后（sse.rs），每条 ToolCallDeltaItem 都携带累计 id/name，
    // 前端 applyToolCallChunk 用真实 id 匹配已有占位卡，不再重复创建。
    const list: ChatItem[] = [];
    // 首 chunk：id + name → 创建占位卡（id = call_abc）
    const s1 = applyToolCallChunk(list, {
      index: 0,
      id: 'call_abc',
      name: 'read_file',
      arguments: '{"pa',
    });
    expect(s1).toHaveLength(1);
    expect(s1[0].toolId).toBe('call_abc');
    expect(s1[0].toolName).toBe('read_file');
    expect(s1[0].toolArgs).toBe('{"pa');
    // 后续 chunk：id + name（累计）+ arguments → 按 id 命中已有卡，就地更新
    const s2 = applyToolCallChunk(s1, {
      index: 0,
      id: 'call_abc',
      name: 'read_file',
      arguments: 'th":"x"}',
    });
    // 不再创建第二张卡（不会出现重复占位卡）
    expect(s2).toHaveLength(1);
    expect(s2[0].toolId).toBe('call_abc');
    expect(s2[0].toolName).toBe('read_file');
    expect(s2[0].toolArgs).toBe('{"path":"x"}');
  });
});

describe('dropStreamPlaceholders', () => {
  it('removes top-level stream placeholder cards (__stream_ prefix)', () => {
    const list: ChatItem[] = [
      { kind: 'user', content: 'hi' },
      tool({ toolId: `${STREAM_TOOL_ID_PREFIX}0`, toolName: 'read_file', toolStatus: 'in_progress' }),
      tool({ toolId: 'real_card', toolName: 'shell', toolResult: 'ok' }),
    ];
    const result = dropStreamPlaceholders(list);
    expect(result).toHaveLength(2);
    expect(result.map((it) => it.toolId ?? it.content)).toEqual(['hi', 'real_card']);
  });

  it('recursively removes stream placeholders inside children', () => {
    const list: ChatItem[] = [
      tool({
        toolId: 'task1',
        toolName: 'Task',
        isSubagent: true,
        children: [
          tool({ toolId: `${STREAM_TOOL_ID_PREFIX}0`, toolName: 'read_file', toolStatus: 'in_progress' }),
          tool({ toolId: 'real_child', toolName: 'shell', toolResult: 'ok' }),
          { kind: 'assistant', content: 'text' },
        ],
      }),
    ];
    const result = dropStreamPlaceholders(list);
    expect(result).toHaveLength(1);
    expect(result[0].children).toHaveLength(2);
    expect(result[0].children!.map((c) => c.toolId ?? c.content)).toEqual(['real_child', 'text']);
  });

  it('preserves non-stream placeholders and non-tool items', () => {
    const list: ChatItem[] = [
      tool({ toolId: 'real', toolName: 'shell' }),
      { kind: 'assistant', content: 'hello' },
      { kind: 'thought', content: 'thinking' },
    ];
    expect(dropStreamPlaceholders(list)).toEqual(list);
  });

  it('returns empty array for empty input', () => {
    expect(dropStreamPlaceholders([])).toEqual([]);
  });

  it('does not mutate the original list (pure function)', () => {
    const child = tool({ toolId: `${STREAM_TOOL_ID_PREFIX}0`, toolName: 'x' });
    const parent = tool({ toolId: 'task1', children: [child] });
    const list = [parent];
    dropStreamPlaceholders(list);
    expect(list[0].children).toHaveLength(1);
  });
});
