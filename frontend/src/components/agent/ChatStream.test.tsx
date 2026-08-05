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
});
