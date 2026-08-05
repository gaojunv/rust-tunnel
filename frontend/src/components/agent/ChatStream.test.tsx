// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach, afterEach, type Mock } from 'vitest';
import { cleanup, render, screen, act } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { listAgentMessages } from '../../api/client';
import ChatStream from './ChatStream';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('../../api/client', () => ({
  listAgentMessages: vi.fn().mockResolvedValue([]),
  updateAgentSessionModel: vi.fn().mockResolvedValue(undefined),
  getAgentDefaultModel: vi.fn().mockResolvedValue(''),
  agentWsUrl: () => 'ws://test/ws',
}));

vi.mock('../../api/agentModels', () => ({
  listAgentSelectableModels: vi.fn().mockResolvedValue({ models: [], groups: [] }),
}));

// 捕获 ws 实例以便手动触发 onmessage
let wsInstance: FakeWs | null = null;
class FakeWs {
  static OPEN = 1;
  readyState = 1;
  sent: string[] = [];
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  constructor() {
    // eslint-disable-next-line @typescript-eslint/no-this-alias -- 捕获实例以便手动触发 onmessage
    wsInstance = this;
  }
  send(s: string) {
    this.sent.push(s);
  }
  close() {}
  emit(msg: object) {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }
}

const renderChat = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ChatStream sessionId="s1" model="" onModelChange={vi.fn()} />
    </QueryClientProvider>
  );
};

describe('ChatStream running state', () => {
  beforeEach(() => {
    vi.stubGlobal('WebSocket', FakeWs as unknown as typeof WebSocket);
    wsInstance = null;
  });
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it('stays running after tool_call, clears on tool_result + done', async () => {
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    act(() => {
      wsInstance!.emit({ type: 'tool_result', id: 'c1', name: 'list_dir', result: 'ok' });
    });
    // tool 回齐但未收到 done → 仍 Running
    expect(screen.getByText('agent.running')).toBeTruthy();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '完成' });
      wsInstance!.emit({ type: 'done' });
    });
    expect(screen.queryByText('agent.running')).toBeNull();
  });

  it('clears running on error even with pending tools', async () => {
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    act(() => {
      wsInstance!.emit({ type: 'error', message: 'boom' });
    });
    expect(screen.queryByText('agent.running')).toBeNull();
  });

  it('force-clears running after 10min timeout', async () => {
    // 说明：vitest v4 + jsdom 环境下，组件模块内部调用的 setTimeout 不会被
    // vi.advanceTimersByTime 驱动（见 task-9 报告），因此改为 spy 捕获 10 分钟
    // 超时回调并确定性触发；断言语义不变：超时兜底必须无条件解除 Running。
    let timeoutCb: (() => void) | undefined;
    const origSetTimeout = globalThis.setTimeout;
    vi.spyOn(globalThis, 'setTimeout').mockImplementation(
      ((cb: () => void, ms?: number) => {
        if (ms === 10 * 60 * 1000) timeoutCb = cb;
        return origSetTimeout(cb, ms ?? 0) as ReturnType<typeof setTimeout>;
      }) as typeof setTimeout,
    );
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    act(() => {
      timeoutCb?.();
    });
    expect(screen.queryByText('agent.running')).toBeNull();
  });

  it('renders new-format tool_calls/tool_result history', async () => {
    (listAgentMessages as Mock).mockResolvedValue([
      { id: 'm1', session_id: 's1', role: 'user', content: '看下文件', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-05' },
      { id: 'm2', session_id: 's1', role: 'assistant', content: '', tool_calls: JSON.stringify([{ id: 'c1', type: 'function', function: { name: 'read_file', arguments: '{"path":"a.rs"}' } }]), tool_call_id: null, name: null, kind: 'tool_calls', created_at: '2026-08-05' },
      { id: 'm3', session_id: 's1', role: 'tool', content: 'fn main(){}', tool_calls: null, tool_call_id: 'c1', name: 'read_file', kind: 'tool_result', created_at: '2026-08-05' },
      { id: 'm4', session_id: 's1', role: 'assistant', content: '文件里是 main 函数', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-05' },
    ]);
    renderChat();
    // 工具名、参数、结果都渲染出来
    expect(await screen.findByText('read_file')).toBeTruthy();
    expect(screen.getByText(/fn main\(\)/)).toBeTruthy();
    expect(screen.getByText('文件里是 main 函数')).toBeTruthy();
  });

  it('renders legacy-format tool row with kind=message role=tool', async () => {
    // 迁移前遗留行：SQLite ALTER TABLE DEFAULT 补 role='tool' 但 kind='message'
    (listAgentMessages as Mock).mockResolvedValue([
      { id: 'm1', session_id: 's1', role: 'tool', content: '', tool_calls: JSON.stringify([{ name: 'shell', args: '{"cmd":"ls"}', result: 'a.rs' }]), tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-05' },
    ]);
    renderChat();
    expect(await screen.findByText('shell')).toBeTruthy();
    expect(screen.getByText('a.rs')).toBeTruthy();
  });

  it('merges streamed assistant_chunk deltas into one bubble', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '你好', final: false });
      wsInstance!.emit({ type: 'assistant_chunk', content: '，世界', final: false });
      wsInstance!.emit({ type: 'assistant_chunk', content: '', final: true });
    });
    // 一个气泡，内容为拼接结果
    const bubbles = screen.getAllByText('你好，世界');
    expect(bubbles).toHaveLength(1);
  });

  it('renders status event as transient hint', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'status', message: 'compacting' });
    });
    expect(await screen.findByText(/compacting|压缩/)).toBeTruthy();
  });

  it('renders non-SSE fallback (content + final in one chunk) as a single bubble', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '非流式回退完整文本', final: true });
    });
    // 一条 content+final:true 消息：先追加内容再关闭气泡 → 单个完整气泡
    const bubbles = screen.getAllByText('非流式回退完整文本');
    expect(bubbles).toHaveLength(1);
  });

  it('status closes the current streaming bubble before appending the hint', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '流式', final: false });
      wsInstance!.emit({ type: 'status', message: 'compacting' });
      wsInstance!.emit({ type: 'assistant_chunk', content: '后续', final: false });
      wsInstance!.emit({ type: 'assistant_chunk', content: '', final: true });
    });
    // status 断开流式气泡：'流式' 与 '后续' 各自独立，不合并
    expect(screen.getByText('流式')).toBeTruthy();
    expect(screen.getByText('后续')).toBeTruthy();
    expect(screen.queryByText('流式后续')).toBeNull();
  });

  it('dedups re-inserted kept segment after compaction (M3)', async () => {
    // DB 物理顺序：[旧消息..., 原kept..., summary, 重插kept...]——压缩修复
    // （801c9a6）使 kept 段以相同内容出现两次，前端必须只渲染一份。
    // K = summary 后行数（含 tool_calls/tool_result 行），跳过 summary 前最后 K 行。
    const row = (id: string, role: string, content: string, kind: string) => ({
      id,
      session_id: 's1',
      role,
      content,
      tool_calls: null,
      tool_call_id: null,
      name: null,
      kind,
      created_at: '2026-08-05',
    });
    const toolCalls = JSON.stringify([
      { id: 'c1', type: 'function', function: { name: 'read_file', arguments: '{"path":"a.rs"}' } },
    ]);
    const toolCallsRow = (id: string) => ({
      ...row(id, 'assistant', '', 'tool_calls'),
      tool_calls: toolCalls,
    });
    const toolResultRow = (id: string) => ({
      ...row(id, 'tool', 'fn main(){}', 'tool_result'),
      tool_call_id: 'c1',
      name: 'read_file',
    });
    (listAgentMessages as Mock).mockResolvedValue([
      row('old1', 'user', '早期问题', 'message'),
      row('old2', 'assistant', '早期回答', 'message'),
      // 原 kept 段（summary 前，含 tool 配对行）——应被跳过
      row('k1', 'user', '保留问题', 'message'),
      toolCallsRow('k2'),
      toolResultRow('k3'),
      row('sum', 'user', '[上下文摘要] 之前讨论了 A', 'summary'),
      // 重插 kept 段（summary 后）——只渲染这一份
      row('k1r', 'user', '保留问题', 'message'),
      toolCallsRow('k2r'),
      toolResultRow('k3r'),
    ]);
    renderChat();
    // 旧消息与 summary 完整保留
    expect(await screen.findByText('早期问题')).toBeTruthy();
    expect(screen.getByText('早期回答')).toBeTruthy();
    expect(screen.getByText('[上下文摘要] 之前讨论了 A')).toBeTruthy();
    // 重插的 kept 段只渲染一次（原始 kept 行被跳过，无连续重复段）
    expect(screen.getAllByText('保留问题')).toHaveLength(1);
    // 工具卡片同样只渲染一份（read_file 工具名 + 结果）
    expect(screen.getAllByText('read_file')).toHaveLength(1);
    expect(screen.getAllByText(/fn main\(\)/)).toHaveLength(1);
  });

  it('renders summary rows as assistant bubbles (M5)', async () => {
    const row = (id: string, role: string, content: string, kind: string) => ({
      id,
      session_id: 's1',
      role,
      content,
      tool_calls: null,
      tool_call_id: null,
      name: null,
      kind,
      created_at: '2026-08-05',
    });
    (listAgentMessages as Mock).mockResolvedValue([
      row('u', 'user', '普通用户消息', 'message'),
      row('s', 'user', '[上下文摘要] 之前讨论了 X', 'summary'),
    ]);
    renderChat();
    const userEl = await screen.findByText('普通用户消息');
    const summaryEl = screen.getByText('[上下文摘要] 之前讨论了 X');
    // summary 走 assistant 气泡样式（mr-auto + bg-muted），而非用户气泡（ml-auto）
    const userBubble = userEl.closest('[class*="ml-auto"]');
    const summaryBubble = summaryEl.closest('[class*="mr-auto"]');
    expect(userBubble?.className).toContain('bg-primary/10');
    expect(summaryBubble?.className).toContain('bg-muted');
  });
});
