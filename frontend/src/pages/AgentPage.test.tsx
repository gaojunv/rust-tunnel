// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import AgentPage from './AgentPage';
import { agentWsUrl } from '../api/client';

const sessions = vi.hoisted(() => ({
  s1: {
    id: 's1',
    workspace_id: 'w1',
    title: 'session one',
    status: 'active',
    created_at: '2026-08-04T00:00:01Z',
    updated_at: '',
  },
  s2: {
    id: 's2',
    workspace_id: 'w1',
    title: 'session two',
    status: 'active',
    created_at: '2026-08-04T00:00:00Z',
    updated_at: '',
  },
}));

const api = vi.hoisted(() => ({
  listAgentSessions: vi.fn(),
  deleteAgentSession: vi.fn(),
}));

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('../api/client', () => ({
  listAgentWorkspaces: vi.fn().mockResolvedValue([
    {
      id: 'w1',
      name: 'proj',
      client_id: 'nas',
      runtime_type: 'host',
      root_path: '/p',
      created_at: '',
      updated_at: '',
    },
  ]),
  listAgentSessions: api.listAgentSessions,
  createAgentSession: vi.fn(),
  deleteAgentSession: api.deleteAgentSession,
  updateAgentSessionTitle: vi.fn(),
  deleteAgentWorkspace: vi.fn(),
  listAgentMessages: vi.fn().mockResolvedValue([]),
  updateAgentSessionModel: vi.fn().mockResolvedValue(undefined),
  getAgentDefaultModel: vi.fn().mockResolvedValue(''),
  putAgentDefaultModel: vi.fn(),
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
  agentWsUrl: vi.fn((sessionId: string) => `ws://test/ws/${sessionId}`),
}));

vi.mock('../api/agentModels', () => ({
  listAgentSelectableModels: vi.fn().mockResolvedValue({ models: [], groups: [] }),
}));

// jsdom 无 WebSocket/ResizeObserver 实现，桩掉（ChatStream 挂载即 new WebSocket）
class FakeWs {
  static OPEN = 1;
  readyState = 1;
  sent: string[] = [];
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  send(s: string) {
    this.sent.push(s);
  }
  close() {}
}

class FakeResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
}

const renderPage = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <AgentPage />
    </QueryClientProvider>
  );
};

describe('AgentPage', () => {
  beforeEach(() => {
    vi.stubGlobal('WebSocket', FakeWs as unknown as typeof WebSocket);
    vi.stubGlobal('ResizeObserver', FakeResizeObserver);
    vi.spyOn(window, 'confirm').mockReturnValue(true);
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('auto-selects the most recent session after selecting a workspace', async () => {
    api.listAgentSessions.mockResolvedValue([sessions.s1, sessions.s2]);

    renderPage();

    // 选中 workspace 后自动选中最近会话（s1）→ ChatStream 挂载并打开其 WS
    await waitFor(() => expect(vi.mocked(agentWsUrl)).toHaveBeenCalledWith('s1'));
    // 顶栏显示当前会话标题
    expect(screen.getByText('session one')).toBeTruthy();
    // 引导态文本不应出现
    expect(screen.queryByText('agent.selectOrNewSession')).toBeNull();
  });

  it('returns to guide state after deleting the current session (no auto-reselect)', async () => {
    api.listAgentSessions
      .mockResolvedValueOnce([sessions.s1, sessions.s2])
      .mockResolvedValue([sessions.s2]);
    api.deleteAgentSession.mockResolvedValue(undefined);

    renderPage();
    await waitFor(() => expect(vi.mocked(agentWsUrl)).toHaveBeenCalledWith('s1'));

    // 打开会话下拉，删除当前会话（s1，列表第一项）
    const trigger = screen.getByLabelText('agent.selectSessionAria');
    fireEvent.pointerDown(trigger);
    const deleteButtons = screen.getAllByLabelText('agent.deleteSession');
    fireEvent.click(deleteButtons[0]);

    await waitFor(() => expect(api.deleteAgentSession).toHaveBeenCalledWith('s1'));

    // 回引导态：不自动重选任何会话（即使 refetch 后列表只剩 s2）
    await waitFor(() => expect(screen.getByText('agent.selectOrNewSession')).toBeTruthy());
    // 不再挂载 ChatStream：WS 只开过一次（初始 s1），不重新打开 s1/s2
    expect(vi.mocked(agentWsUrl)).toHaveBeenCalledTimes(1);
    // 顶栏回到「选择会话」占位
    expect(screen.getByText('agent.selectSession')).toBeTruthy();
  });
});
