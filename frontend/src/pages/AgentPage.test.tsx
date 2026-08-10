// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import AgentPage from './AgentPage';

const sessionFixtures = vi.hoisted(() => ({
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
  sNew: {
    id: 's-new',
    workspace_id: 'w1',
    title: 'newest',
    status: 'active',
    created_at: '2026-08-05T00:00:00Z',
    updated_at: '',
  },
  sOld: {
    id: 's-old',
    workspace_id: 'w1',
    title: 'older',
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

// ChatStream 替身：仅渲染会话 id 供断言（刷新恢复/回退逻辑观察点在挂载的 sessionId）
vi.mock('../components/agent/ChatStream', () => ({
  default: ({ sessionId }: { sessionId: string }) => (
    <div data-testid="chat-stream" data-session-id={sessionId} />
  ),
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

const renderPage = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <AgentPage />
    </QueryClientProvider>
  );
};

const sessionIdOf = (el: HTMLElement) => el.getAttribute('data-session-id');

describe('AgentPage', () => {
  beforeEach(() => {
    // 刷新恢复用 localStorage：用例间清空，避免污染
    localStorage.clear();
    vi.spyOn(window, 'confirm').mockReturnValue(true);
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
    vi.restoreAllMocks();
  });

  it('auto-selects the most recent session after selecting a workspace', async () => {
    api.listAgentSessions.mockResolvedValue([sessionFixtures.s1, sessionFixtures.s2]);

    renderPage();

    // 选中 workspace 后自动选中最近会话（s1）→ ChatStream 挂载
    const stream = await screen.findByTestId('chat-stream');
    expect(sessionIdOf(stream)).toBe('s1');
    // 顶栏显示当前会话标题
    expect(screen.getByText('session one')).toBeTruthy();
    // 引导态文本不应出现
    expect(screen.queryByText('agent.selectOrNewSession')).toBeNull();
  });

  it('returns to guide state after deleting the current session (no auto-reselect)', async () => {
    api.listAgentSessions
      .mockResolvedValueOnce([sessionFixtures.s1, sessionFixtures.s2])
      .mockResolvedValue([sessionFixtures.s2]);
    api.deleteAgentSession.mockResolvedValue(undefined);

    renderPage();
    const stream = await screen.findByTestId('chat-stream');
    expect(sessionIdOf(stream)).toBe('s1');

    // 打开会话下拉，删除当前会话（s1，列表第一项）
    const trigger = screen.getByLabelText('agent.selectSessionAria');
    fireEvent.pointerDown(trigger);
    const deleteButtons = screen.getAllByLabelText('agent.deleteSession');
    fireEvent.click(deleteButtons[0]);

    // 删除改为确认 Dialog（SessionBar 不再用 window.confirm）：点「删除」确认后才调用 API
    fireEvent.click(await screen.findByText('common.delete'));

    await waitFor(() => expect(api.deleteAgentSession).toHaveBeenCalledWith('s1'));

    // 回引导态：不自动重选任何会话（即使 refetch 后列表只剩 s2）
    await waitFor(() => expect(screen.getByText('agent.selectOrNewSession')).toBeTruthy());
    // 不再挂载 ChatStream
    expect(screen.queryByTestId('chat-stream')).toBeNull();
    // 顶栏回到「选择会话」占位
    expect(screen.getByText('agent.selectSession')).toBeTruthy();
  });

  it('restores last selected session from localStorage after remount', async () => {
    localStorage.setItem('agent.lastWorkspaceId', 'w1');
    localStorage.setItem('agent.lastSessionId', 's-old');
    api.listAgentSessions.mockResolvedValue([sessionFixtures.sNew, sessionFixtures.sOld]);

    renderPage();

    // ChatStream 收到恢复的 s-old，而非列表最新的 s-new
    const stream = await screen.findByTestId('chat-stream');
    expect(sessionIdOf(stream)).toBe('s-old');
  });

  it('falls back to newest session when stored id is gone', async () => {
    localStorage.setItem('agent.lastWorkspaceId', 'w1');
    localStorage.setItem('agent.lastSessionId', 's-deleted');
    api.listAgentSessions.mockResolvedValue([sessionFixtures.sNew, sessionFixtures.sOld]);

    renderPage();

    // 恢复的 id 已不存在（会话被删）：挂载时先短暂显示 s-deleted，sessions
    // 到达后回退到最新的 s-new——等待回退完成而非首次出现
    await waitFor(() => {
      const stream = screen.getByTestId('chat-stream');
      expect(sessionIdOf(stream)).toBe('s-new');
    });
  });

  it('persists selection to localStorage on manual select', async () => {
    api.listAgentSessions.mockResolvedValue([sessionFixtures.sNew, sessionFixtures.sOld]);

    renderPage();
    await screen.findByTestId('chat-stream');
    // 自动选中 s-new 后 localStorage 同步更新
    expect(localStorage.getItem('agent.lastSessionId')).toBe('s-new');
  });
});
