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
    model: 'sonnet',
    created_at: '2026-08-04T00:00:01Z',
    updated_at: '',
  },
  s2: {
    id: 's2',
    workspace_id: 'w1',
    title: 'session two',
    status: 'active',
    model: 'haiku',
    created_at: '2026-08-04T00:00:00Z',
    updated_at: '',
  },
  sNew: {
    id: 's-new',
    workspace_id: 'w1',
    title: 'newest',
    status: 'active',
    model: 'opus',
    created_at: '2026-08-05T00:00:00Z',
    updated_at: '',
  },
  sOld: {
    id: 's-old',
    workspace_id: 'w1',
    title: 'older',
    status: 'active',
    model: 'sonnet',
    created_at: '2026-08-04T00:00:00Z',
    updated_at: '',
  },
}));

const api = vi.hoisted(() => ({
  listAgentSessions: vi.fn(),
  deleteAgentSession: vi.fn(),
  createAgentSession: vi.fn(),
}));

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

// ChatStream 替身：渲染会话 id/model/active 供断言。多 tab 下会同时挂载多个实例，
// 断言用 getAllByTestId 按 data-session-id 区分。
vi.mock('../components/agent/ChatStream', () => ({
  default: (props: { sessionId: string; model?: string; active?: boolean }) => (
    <div
      data-testid="chat-stream"
      data-session-id={props.sessionId}
      data-model={props.model ?? ''}
      data-active={props.active ?? false}
    />
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
  createAgentSession: api.createAgentSession,
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

// 通知上下文替身：AgentPage 仅上报 activeSessionId，断言不涉及具体行为
vi.mock('../notifications/NotificationProvider', () => ({
  useAgentNotifications: () => ({
    enabled: true,
    permission: 'default',
    setEnabled: vi.fn(),
    setActiveSessionId: vi.fn(),
  }),
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

/** 激活（可见）的 ChatStream 实例：外层包裹 div className = h-full。 */
const visibleStream = () => {
  const streams = screen.getAllByTestId('chat-stream');
  return streams.find((el) => el.parentElement?.className === 'h-full');
};

/** 打开 SessionBar 下拉（Radix 菜单 pointerdown 打开；portal 渲染到 body）。 */
const openSessionMenu = async () => {
  const trigger = screen.getByLabelText('agent.selectSessionAria');
  fireEvent.pointerDown(trigger);
  fireEvent.click(trigger);
  await screen.findByText('agent.newSession');
};

/** 从 SessionBar 下拉选择某个会话（打开/激活对应 tab）。 */
const openSessionFromMenu = async (title: string) => {
  await openSessionMenu();
  fireEvent.click(await screen.findByText(title));
};

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

  it('seeds a single tab with the most recent session after selecting a workspace', async () => {
    api.listAgentSessions.mockResolvedValue([sessionFixtures.s1, sessionFixtures.s2]);

    renderPage();

    // 选中 workspace 后播种最近会话（s1）→ 单个 ChatStream 挂载
    const streams = await screen.findAllByTestId('chat-stream');
    expect(streams).toHaveLength(1);
    expect(sessionIdOf(streams[0])).toBe('s1');
    // 标签栏 + 顶栏回显当前会话标题
    expect(screen.getAllByText('session one').length).toBeGreaterThan(0);
    // 引导态文本不应出现
    expect(screen.queryByText('agent.selectOrNewSession')).toBeNull();
  });

  it('returns to guide state after deleting the active session (no auto-reselect)', async () => {
    api.listAgentSessions.mockResolvedValue([sessionFixtures.s1, sessionFixtures.s2]);
    api.deleteAgentSession.mockResolvedValue(undefined);

    renderPage();
    const stream = await screen.findByTestId('chat-stream');
    expect(sessionIdOf(stream)).toBe('s1');

    // 打开会话下拉，删除当前会话（s1，列表第一项）
    await openSessionMenu();
    const deleteButtons = screen.getAllByLabelText('agent.deleteSession');
    fireEvent.click(deleteButtons[0]);

    // 删除改为确认 Dialog（SessionBar 不再用 window.confirm）：点「删除」确认后才调用 API
    fireEvent.click(await screen.findByText('common.delete'));

    await waitFor(() => expect(api.deleteAgentSession).toHaveBeenCalledWith('s1'));

    // 回引导态：删会话关 tab，且不自动重选（reconcile 只过滤、不会重新播种）
    await waitFor(() => expect(screen.getByText('agent.selectOrNewSession')).toBeTruthy());
    expect(screen.queryByTestId('chat-stream')).toBeNull();
    // 顶栏回到「选择会话」占位
    expect(screen.getByText('agent.selectSession')).toBeTruthy();
  });

  it('migrates legacy lastSessionId into a single tab on remount', async () => {
    localStorage.setItem('agent.lastWorkspaceId', 'w1');
    localStorage.setItem('agent.lastSessionId', 's-old');
    api.listAgentSessions.mockResolvedValue([sessionFixtures.sNew, sessionFixtures.sOld]);

    renderPage();

    // ChatStream 收到迁移的 s-old，而非列表最新的 s-new
    const stream = await screen.findByTestId('chat-stream');
    expect(sessionIdOf(stream)).toBe('s-old');
    // 迁移后旧 key 被删除，新 key 持久化
    expect(localStorage.getItem('agent.lastSessionId')).toBeNull();
    await waitFor(() => {
      expect(JSON.parse(localStorage.getItem('agent.openTabs.w1') ?? '{}')).toEqual({
        open: ['s-old'],
        active: 's-old',
      });
    });
  });

  it('falls back to newest session when stored id is gone', async () => {
    localStorage.setItem('agent.lastWorkspaceId', 'w1');
    localStorage.setItem('agent.lastSessionId', 's-deleted');
    api.listAgentSessions.mockResolvedValue([sessionFixtures.sNew, sessionFixtures.sOld]);

    renderPage();

    // 迁移的 id 已不存在（会话被删）：过滤为空后回退播种最新的 s-new
    await waitFor(() => {
      const stream = screen.getByTestId('chat-stream');
      expect(sessionIdOf(stream)).toBe('s-new');
    });
  });

  it('persists tab state to localStorage after seeding', async () => {
    api.listAgentSessions.mockResolvedValue([sessionFixtures.sNew, sessionFixtures.sOld]);

    renderPage();
    await screen.findByTestId('chat-stream');

    // 播种 s-new 后同步持久化到 agent.openTabs.<workspaceId>
    expect(JSON.parse(localStorage.getItem('agent.openTabs.w1') ?? '{}')).toEqual({
      open: ['s-new'],
      active: 's-new',
    });
  });

  it('keeps multiple tabs mounted with the active one visible (hidden keeps streaming)', async () => {
    // 可变会话列表 + 新建回填：SessionTabBar 挂载触发的 refetch 不会把乐观写入
    // 的 s-new 覆盖掉（列表始终包含它）。
    const sessionsList = [sessionFixtures.s1, sessionFixtures.s2];
    api.listAgentSessions.mockImplementation(() => Promise.resolve([...sessionsList]));
    api.createAgentSession.mockImplementation(async () => {
      const s = sessionFixtures.sNew;
      sessionsList.unshift(s);
      return s;
    });

    renderPage();
    await screen.findByTestId('chat-stream'); // s1 播种
    await openSessionMenu();
    fireEvent.click(await screen.findByText('agent.newSession'));

    // 新会话 → 新 tab 激活，旧 tab 保持挂载（hidden）
    await waitFor(() => {
      expect(screen.getAllByTestId('chat-stream')).toHaveLength(2);
    });
    const streams = screen.getAllByTestId('chat-stream');
    const byId = Object.fromEntries(streams.map((el) => [sessionIdOf(el), el]));
    expect(byId['s-new']).toBeTruthy();
    expect(byId['s1']).toBeTruthy();
    expect(byId['s-new'].parentElement?.className).toContain('h-full');
    expect(byId['s1'].parentElement?.className).toContain('hidden');
  });

  it('closing the active tab activates its right neighbor', async () => {
    api.listAgentSessions.mockResolvedValue([sessionFixtures.s1, sessionFixtures.s2]);

    renderPage();
    await screen.findByTestId('chat-stream'); // s1 播种并激活

    // 从下拉打开 s2 → open=[s1,s2]，s2 激活
    await openSessionFromMenu('session two');
    await waitFor(() => {
      expect(screen.getAllByTestId('chat-stream')).toHaveLength(2);
      expect(sessionIdOf(visibleStream()!)).toBe('s2');
    });

    // 切回 s1，再关闭 s1 → 右侧邻居 s2 激活
    fireEvent.click(screen.getByRole('tab', { name: 'session one' }));
    await waitFor(() => expect(sessionIdOf(visibleStream()!)).toBe('s1'));
    fireEvent.click(screen.getAllByLabelText('agent.closeTab')[0]); // s1 的 ×
    await waitFor(() => {
      const streams = screen.getAllByTestId('chat-stream');
      expect(streams).toHaveLength(1);
      expect(sessionIdOf(streams[0])).toBe('s2');
    });
  });

  it('reopens a closed tab from the SessionBar dropdown (session data preserved)', async () => {
    api.listAgentSessions.mockResolvedValue([sessionFixtures.s1, sessionFixtures.s2]);

    renderPage();
    await screen.findByTestId('chat-stream'); // s1

    // 打开 s2 → 关闭 s2 → 标签消失但会话仍在列表
    await openSessionFromMenu('session two');
    await waitFor(() => expect(screen.getAllByTestId('chat-stream')).toHaveLength(2));
    fireEvent.click(screen.getAllByLabelText('agent.closeTab')[1]); // s2 的 ×
    await waitFor(() => expect(screen.getAllByTestId('chat-stream')).toHaveLength(1));

    // 从 SessionBar 下拉重新打开 s2
    await openSessionMenu();
    fireEvent.click(await screen.findByText('session two'));
    await waitFor(() => expect(screen.getAllByTestId('chat-stream')).toHaveLength(2));
  });

  it('passes per-session model to each tab', async () => {
    api.listAgentSessions.mockResolvedValue([sessionFixtures.s1, sessionFixtures.s2]);

    renderPage();
    await screen.findByTestId('chat-stream'); // s1 = sonnet

    await openSessionFromMenu('session two');
    await waitFor(() => expect(screen.getAllByTestId('chat-stream')).toHaveLength(2));

    const streams = screen.getAllByTestId('chat-stream');
    const byId = Object.fromEntries(streams.map((el) => [sessionIdOf(el), el]));
    expect(byId['s1'].getAttribute('data-model')).toBe('sonnet');
    expect(byId['s2'].getAttribute('data-model')).toBe('haiku');
  });
});
