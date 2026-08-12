import { describe, expect, it } from 'vitest';
import type { AgentMessage } from '../../types';
import { historyToChatItems } from './history';

const row = (overrides: Partial<AgentMessage> & { id: string }): AgentMessage => ({
  session_id: 's1',
  role: 'assistant',
  content: '',
  tool_calls: null,
  tool_call_id: null,
  name: null,
  kind: 'message',
  created_at: '2026-08-12',
  ...overrides,
});

describe('historyToChatItems', () => {
  it('dedups multiple tool_result rows of one tool_call_id into a single card (terminal content)', () => {
    // 服务端历史上同一 tool_call_id 落库多行：1 条 tool_calls + N 条 tool_result，
    // 中间态 content 为空。旧逻辑每行渲染一张卡 → 同一工具多张卡、live 匹配只
    // patch 第一张、其余残留 running（Bug 复现）。
    const calls = JSON.stringify([{ id: 'c1', name: 'list_dir', arguments: '{"path":"."}' }]);
    const items = historyToChatItems([
      row({ id: 'm1', role: 'user', content: '看下目录' }),
      row({ id: 'm2', kind: 'tool_calls', tool_calls: calls, tool_call_id: 'c1', name: 'list_dir' }),
      row({ id: 'm3', kind: 'tool_result', tool_call_id: 'c1', role: 'tool', name: 'list_dir', content: '' }),
      row({ id: 'm4', kind: 'tool_result', tool_call_id: 'c1', role: 'tool', name: 'list_dir', content: '' }),
      row({ id: 'm5', kind: 'tool_result', tool_call_id: 'c1', role: 'tool', name: 'list_dir', content: 'src/ tests/' }),
      row({ id: 'm6', content: '完成' }),
    ]);
    // 恰好 1 张工具卡，content 为终态
    const tools = items.filter((it) => it.kind === 'tool');
    expect(tools).toHaveLength(1);
    expect(tools[0]).toMatchObject({
      toolId: 'c1',
      toolName: 'list_dir',
      toolArgs: '{"path":"."}',
      toolResult: 'src/ tests/',
      toolStatus: 'completed',
    });
    // 普通消息照常渲染
    expect(items.some((it) => it.kind === 'user' && it.content === '看下目录')).toBe(true);
    expect(items.some((it) => it.kind === 'assistant' && it.content === '完成')).toBe(true);
  });

  it('takes the last non-empty tool_result when multiple rows carry content', () => {
    const calls = JSON.stringify([{ id: 'c1', name: 'read_file', arguments: '{"path":"a.rs"}' }]);
    const items = historyToChatItems([
      row({ id: 'm1', kind: 'tool_calls', tool_calls: calls, tool_call_id: 'c1', name: 'read_file' }),
      row({ id: 'm2', kind: 'tool_result', tool_call_id: 'c1', role: 'tool', name: 'read_file', content: '第一版' }),
      row({ id: 'm3', kind: 'tool_result', tool_call_id: 'c1', role: 'tool', name: 'read_file', content: '最终结果' }),
    ]);
    const tools = items.filter((it) => it.kind === 'tool');
    expect(tools).toHaveLength(1);
    expect(tools[0].toolResult).toBe('最终结果');
  });

  it('renders orphan tool_calls row as failed placeholder card (turn interrupted mid-tool)', () => {
    // 回合在工具执行中被刷新/断线打断：tool_call 已落库，tool_result 永不到达。
    // 无配对 → failed 占位卡，否则该工具从聊天区彻底消失。
    const calls = JSON.stringify([{ id: 'c1', name: 'list_dir', arguments: '{"path":"."}' }]);
    const items = historyToChatItems([
      row({ id: 'm1', role: 'user', content: '看下目录' }),
      row({ id: 'm2', kind: 'tool_calls', tool_calls: calls, tool_call_id: 'c1', name: 'list_dir' }),
    ]);
    const tools = items.filter((it) => it.kind === 'tool');
    expect(tools).toHaveLength(1);
    expect(tools[0].toolStatus).toBe('failed');
    expect(tools[0].toolResult).toBeUndefined();
    expect(tools[0].toolId).toBe('c1');
    expect(tools[0].toolName).toBe('list_dir');
  });

  it('does not render orphan failed card for tool_calls paired with tool_result', () => {
    // 正常完成的工具：tool_calls 行有配对 tool_result 时跳过（args 由 tool_result
    // 卡片展示），只出一张 completed 卡，不重复。
    const calls = JSON.stringify([{ id: 'c1', name: 'list_dir', arguments: '{"path":"."}' }]);
    const items = historyToChatItems([
      row({ id: 'm1', kind: 'tool_calls', tool_calls: calls, tool_call_id: 'c1', name: 'list_dir' }),
      row({ id: 'm2', kind: 'tool_result', tool_call_id: 'c1', role: 'tool', name: 'list_dir', content: 'src/' }),
    ]);
    const tools = items.filter((it) => it.kind === 'tool');
    expect(tools).toHaveLength(1);
    expect(tools[0].toolStatus).toBe('completed');
  });

  it('does not render orphan failed card for runner-format call paired with tool_result', () => {
    // runner 旧格式：tool_calls 行整行 tool_call_id 列为空，但 JSON 内带 id。
    // 按 JSON 内 id 与 tool_result 配对——配对的跳过 failed 卡。
    const calls = JSON.stringify([{ id: 'c1', name: 'list_dir', arguments: '{"path":"."}' }]);
    const items = historyToChatItems([
      row({ id: 'm1', kind: 'tool_calls', tool_calls: calls }), // tool_call_id 列为空
      row({ id: 'm2', kind: 'tool_result', tool_call_id: 'c1', role: 'tool', name: 'list_dir', content: 'src/' }),
    ]);
    const tools = items.filter((it) => it.kind === 'tool');
    expect(tools).toHaveLength(1);
    expect(tools[0].toolStatus).toBe('completed');
  });

  it('renders orphan runner-format call (column tool_call_id null) as failed card', () => {
    const calls = JSON.stringify([{ id: 'c1', name: 'list_dir', arguments: '{"path":"."}' }]);
    const items = historyToChatItems([
      row({ id: 'm1', kind: 'tool_calls', tool_calls: calls }), // 无 tool_result 配对
    ]);
    const tools = items.filter((it) => it.kind === 'tool');
    expect(tools).toHaveLength(1);
    expect(tools[0].toolStatus).toBe('failed');
  });

  it('still merges legacy tool_log rows (kind=message role=tool)', () => {
    // 迁移前遗留行：SQLite ALTER TABLE DEFAULT 补 role='tool' 但 kind='message'，
    // 整行 tool_call_id 列为空，tool_calls JSON 内含 name/args/result。
    const items = historyToChatItems([
      row({
        id: 'm1',
        role: 'tool',
        kind: 'message',
        tool_calls: JSON.stringify([{ name: 'shell', args: '{"cmd":"ls"}', result: 'a.rs' }]),
      }),
    ]);
    const tools = items.filter((it) => it.kind === 'tool');
    expect(tools).toHaveLength(1);
    expect(tools[0]).toMatchObject({
      toolName: 'shell',
      toolArgs: '{"cmd":"ls"}',
      toolResult: 'a.rs',
    });
  });

  it('renders only last plan, summary as assistant bubble, normal messages unchanged', () => {
    const items = historyToChatItems([
      row({ id: 'm1', role: 'user', content: '早' }),
      row({ id: 'm2', name: 'plan', kind: 'message', content: JSON.stringify([{ content: '旧计划', status: 'pending' }]) }),
      row({ id: 'm3', name: 'plan', kind: 'message', content: JSON.stringify([{ content: '新计划', status: 'completed' }]) }),
      row({ id: 'm4', role: 'user', kind: 'summary', content: '[上下文摘要] 之前讨论了 X' }),
      row({ id: 'm5', content: '回答' }),
    ]);
    // 多条 plan 只留最后一条（ACP plan 全量替换语义）
    const plans = items.filter((it) => it.kind === 'plan');
    expect(plans).toHaveLength(1);
    expect(plans[0].planEntries).toEqual([{ content: '新计划', status: 'completed' }]);
    // summary 渲染为 assistant 气泡（muted 样式）
    expect(items.filter((it) => it.kind === 'assistant' && it.content === '[上下文摘要] 之前讨论了 X')).toHaveLength(1);
    // 普通消息不回归
    expect(items.some((it) => it.kind === 'user' && it.content === '早')).toBe(true);
    expect(items.some((it) => it.kind === 'assistant' && it.content === '回答')).toBe(true);
  });

  it('dedups re-inserted kept segment after compaction (M3)', () => {
    // DB 物理顺序：[旧消息..., 原kept..., summary, 重插kept...]——压缩修复（801c9a6）
    // 使 kept 段以相同内容出现两次，前端必须只渲染一份。summary 后的重插段是 kept
    // 段原样复制，按内容匹配跳过 summary 前的原件。
    const calls = JSON.stringify([{ id: 'c1', type: 'function', function: { name: 'read_file', arguments: '{"path":"a.rs"}' } }]);
    const toolCallsRow = (id: string) => row({ id, kind: 'tool_calls', tool_calls: calls });
    const toolResultRow = (id: string) =>
      row({ id, kind: 'tool_result', tool_call_id: 'c1', role: 'tool', name: 'read_file', content: 'fn main(){}' });
    const items = historyToChatItems([
      row({ id: 'old1', role: 'user', content: '早期问题' }),
      row({ id: 'old2', content: '早期回答' }),
      row({ id: 'k1', role: 'user', content: '保留问题' }),
      toolCallsRow('k2'),
      toolResultRow('k3'),
      row({ id: 'sum', role: 'user', kind: 'summary', content: '[上下文摘要] 之前讨论了 A' }),
      row({ id: 'k1r', role: 'user', content: '保留问题' }),
      toolCallsRow('k2r'),
      toolResultRow('k3r'),
    ]);
    // 原始 kept 段被跳过，重插段只渲染一份
    expect(items.filter((it) => it.kind === 'user' && it.content === '保留问题')).toHaveLength(1);
    expect(items.filter((it) => it.kind === 'tool')).toHaveLength(1);
    expect(items.filter((it) => it.kind === 'assistant' && it.content === '早期回答')).toHaveLength(1);
    expect(items.some((it) => it.kind === 'assistant' && it.content === '[上下文摘要] 之前讨论了 A')).toBe(true);
  });
});
