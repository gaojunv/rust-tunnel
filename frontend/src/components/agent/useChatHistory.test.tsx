// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { cleanup, render, screen, act, fireEvent } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { listAgentMessages } from '../../api/client';
import ChatStream from './ChatStream';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('../../api/client', () => ({
  listAgentMessages: vi.fn().mockResolvedValue({ messages: [], has_more: false }),
  updateAgentSessionModel: vi.fn().mockResolvedValue(undefined),
  getAgentDefaultModel: vi.fn().mockResolvedValue(''),
  listWorkspaceFiles: vi.fn().mockResolvedValue({ files: [] }),
  agentWsUrl: () => 'ws://test/ws',
}));

vi.mock('../../api/agentModels', () => ({
  listAgentSelectableModels: vi.fn().mockResolvedValue({ models: [], groups: [] }),
}));

class FakeWs {
  static OPEN = 1;
  readyState = 1;
  sent: string[] = [];
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onopen: (() => void) | null = null;
  constructor() {}
  send(s: string) {
    this.sent.push(s);
  }
  close() {}
  emit(msg: object) {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }
}

const renderChat = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false, refetchOnMount: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ChatStream sessionId="s1" workspaceId="w1" model="" onModelChange={vi.fn()} />
    </QueryClientProvider>,
  );
};

describe('useChatHistory 分页合并', () => {
  beforeEach(() => {
    vi.stubGlobal('WebSocket', FakeWs as unknown as typeof WebSocket);
  });
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('分页合并：首页 + 加载更早 = 顺序正确', async () => {
    const row = (i: number) => ({
      id: `m${i}`,
      session_id: 's1',
      role: 'user' as const,
      content: `消息 ${i}`,
      tool_calls: null,
      tool_call_id: null,
      name: null,
      kind: 'message' as const,
      created_at: '2026-08-05',
    });
    vi.mocked(listAgentMessages)
      .mockResolvedValueOnce({ messages: [row(3), row(4), row(5)], has_more: true } as never)
      .mockResolvedValue({ messages: [row(0), row(1), row(2)], has_more: false } as never);
    renderChat();
    expect(await screen.findByText('消息 3')).toBeTruthy();
    await act(async () => {
      fireEvent.click(screen.getByText('agent.loadEarlierMessages'));
    });
    expect(await screen.findByText('消息 0')).toBeTruthy();
    const msgs = screen
      .getAllByText(/^消息 \d$/)
      .map((el) => el.textContent)
      .filter((x): x is string => x !== null);
    expect(msgs).toEqual(['消息 0', '消息 1', '消息 2', '消息 3', '消息 4', '消息 5']);
  });

  it('hasMore 翻转后按钮显隐变化', async () => {
    const row = (i: number) => ({
      id: `m${i}`,
      session_id: 's1',
      role: 'user' as const,
      content: `消息 ${i}`,
      tool_calls: null,
      tool_call_id: null,
      name: null,
      kind: 'message' as const,
      created_at: '2026-08-05',
    });
    vi.mocked(listAgentMessages)
      .mockResolvedValueOnce({ messages: [row(10)], has_more: true } as never)
      .mockResolvedValue({ messages: [row(9)], has_more: false } as never);
    renderChat();
    expect(await screen.findByText('消息 10')).toBeTruthy();
    expect(screen.getByText('agent.loadEarlierMessages')).toBeTruthy();
    await act(async () => {
      fireEvent.click(screen.getByText('agent.loadEarlierMessages'));
    });
    expect(await screen.findByText('消息 9')).toBeTruthy();
    expect(screen.queryByText('agent.loadEarlierMessages')).toBeNull();
  });

  it('切菜单再切回（同 QueryClient 重挂载）：权威 refetch 覆盖过期缓存快照，新消息不丢', async () => {
    const row = (i: number) => ({
      id: `m${i}`,
      session_id: 's1',
      role: 'user' as const,
      content: `消息 ${i}`,
      tool_calls: null,
      tool_call_id: null,
      name: null,
      kind: 'message' as const,
      created_at: '2026-08-05',
    });
    // 共享 QueryClient：模拟路由切换（组件卸载/重挂载）时查询缓存存活
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const renderWithSharedCache = () =>
      render(
        <QueryClientProvider client={qc}>
          <ChatStream sessionId="s1" workspaceId="w1" model="" onModelChange={vi.fn()} />
        </QueryClientProvider>,
      );

    // 首次访问：2 条消息落入缓存快照
    vi.mocked(listAgentMessages).mockResolvedValueOnce({ messages: [row(0), row(1)], has_more: false } as never);
    renderWithSharedCache();
    expect(await screen.findByText('消息 0')).toBeTruthy();
    expect(await screen.findByText('消息 1')).toBeTruthy();
    expect(screen.queryByText('消息 2')).toBeNull();

    cleanup(); // 切走菜单：组件卸载，缓存快照（2 条）存活

    // 切走期间服务端新增 1 条消息
    vi.mocked(listAgentMessages).mockResolvedValueOnce({ messages: [row(0), row(1), row(2)], has_more: false } as never);

    // 切回来重挂载：首帧上屏缓存快照（2 条），权威 refetch 返回 3 条——必须覆盖，否则 消息 2 丢失
    renderWithSharedCache();
    expect(await screen.findByText('消息 2')).toBeTruthy();
    expect(screen.getByText('消息 0')).toBeTruthy();
    expect(screen.getByText('消息 1')).toBeTruthy();
  });
});
