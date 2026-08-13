// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import SessionBar from './SessionBar';
import { formatRelativeTime } from './formatRelativeTime';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const api = vi.hoisted(() => ({
  listAgentSessions: vi.fn(),
  deleteAgentSession: vi.fn(),
  updateAgentSessionTitle: vi.fn(),
}));

vi.mock('../../api/client', () => ({
  listAgentSessions: api.listAgentSessions,
  deleteAgentSession: api.deleteAgentSession,
  updateAgentSessionTitle: api.updateAgentSessionTitle,
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

const minutesAgo = (n: number) => new Date(Date.now() - n * 60_000).toISOString();

const sessions = [
  // 带标题 + 模型 + 3 分钟前更新 → 下拉显示「x 分钟前 · 模型」
  {
    id: 's1',
    workspace_id: 'w1',
    title: '修复登录',
    status: 'active',
    model: 'sonnet',
    created_at: minutesAgo(60),
    updated_at: minutesAgo(3),
  },
  // 无标题无模型 + 2 小时前更新 → 仅显示相对时间
  {
    id: 's2',
    workspace_id: 'w1',
    title: null,
    status: 'active',
    created_at: minutesAgo(120),
    updated_at: minutesAgo(120),
  },
];

beforeEach(() => {
  api.listAgentSessions.mockResolvedValue(sessions);
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const renderBar = (sessionId = 's1') => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const handlers = { onSelect: vi.fn(), onSessionDeleted: vi.fn(), onNew: vi.fn() };
  return {
    handlers,
    ...render(
      <QueryClientProvider client={qc}>
        <SessionBar
          workspaceId="w1"
          sessionId={sessionId}
          {...handlers}
        />
      </QueryClientProvider>,
    ),
  };
};

/** 打开下拉（Radix 菜单在 pointerdown 时打开；portal 渲染到 body）。 */
const openMenu = async () => {
  const trigger = await screen.findByRole('button', { name: 'agent.selectSessionAria' });
  fireEvent.pointerDown(trigger);
  fireEvent.click(trigger);
  await screen.findByText('agent.newSession');
};

describe('SessionBar', () => {
  it('shows current session title on trigger', async () => {
    renderBar('s1');
    expect(await screen.findByText('修复登录')).toBeTruthy();
  });

  it('falls back to unnamed label for untitled session', async () => {
    renderBar('s2');
    expect(await screen.findByText('agent.unnamedSession')).toBeTruthy();
  });

  it('dropdown shows relative time + model for each session', async () => {
    renderBar('s1');
    await openMenu();
    // 带模型的会话：第二行 = 相对时间 · 模型
    expect(await screen.findByText('agent.timeMinutesAgo · sonnet')).toBeTruthy();
    // 无模型会话：第二行仅相对时间（2 小时 → 小时级 bucket）
    expect(screen.getByText('agent.timeHoursAgo')).toBeTruthy();
    // 新建收进下拉：仅 sticky 新建会话项（顶栏不再有独立按钮）
    expect(screen.getByText('agent.newSession')).toBeTruthy();
  });

  it('new session item in dropdown invokes onNew', async () => {
    const onNew = vi.fn();
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <SessionBar
          workspaceId="w1"
          sessionId="s1"
          onSelect={vi.fn()}
          onSessionDeleted={vi.fn()}
          onNew={onNew}
        />
      </QueryClientProvider>
    );
    await openMenu();
    fireEvent.click(await screen.findByText('agent.newSession'));
    expect(onNew).toHaveBeenCalled();
  });

  it('deletes a session via Dialog confirm (not window.confirm)', async () => {
    const { handlers } = renderBar('s1');
    await openMenu();
    const deleteButtons = await screen.findAllByLabelText('agent.deleteSession');
    fireEvent.click(deleteButtons[0]); // s1（当前会话）
    // 确认 Dialog 弹出并显示会话标题
    expect(await screen.findByText('agent.confirmDeleteSessionTitle')).toBeTruthy();
    expect(screen.getByText('agent.confirmDeleteSessionDesc')).toBeTruthy();
    // 取消：不触发删除
    fireEvent.click(screen.getByText('common.cancel'));
    expect(api.deleteAgentSession).not.toHaveBeenCalled();
    // 再次触发并确认 → 调用 API，并回调被删会话 id
    await openMenu();
    const buttons = await screen.findAllByLabelText('agent.deleteSession');
    fireEvent.click(buttons[0]);
    fireEvent.click(await screen.findByText('common.delete'));
    await waitFor(() => {
      expect(api.deleteAgentSession).toHaveBeenCalledWith('s1');
    });
    expect(handlers.onSessionDeleted).toHaveBeenCalledWith('s1');
  });

  it('invokes onSessionDeleted even when deleting a non-current session', async () => {
    const { handlers } = renderBar('s1');
    await openMenu();
    // 删除非当前会话 s2（列表第二项）
    const deleteButtons = await screen.findAllByLabelText('agent.deleteSession');
    fireEvent.click(deleteButtons[1]);
    fireEvent.click(await screen.findByText('common.delete'));
    await waitFor(() => {
      expect(api.deleteAgentSession).toHaveBeenCalledWith('s2');
    });
    expect(handlers.onSessionDeleted).toHaveBeenCalledWith('s2');
  });
});

describe('formatRelativeTime', () => {
  // 固定基准时间，避免测试间时序抖动
  const now = 1_750_000_000_000;
  const t = (k: string) => k;

  it('刚发生（<60s）→ just now；未来时间也归为 just now', () => {
    expect(formatRelativeTime(now, now, t)).toBe('agent.timeJustNow');
    expect(formatRelativeTime(now - 59_000, now, t)).toBe('agent.timeJustNow');
    expect(formatRelativeTime(now + 5_000, now, t)).toBe('agent.timeJustNow');
  });

  it('分钟级（<60min）', () => {
    expect(formatRelativeTime(now - 3 * 60_000, now, t)).toBe('agent.timeMinutesAgo');
    expect(formatRelativeTime(now - 59 * 60_000, now, t)).toBe('agent.timeMinutesAgo');
  });

  it('小时级（<24h）', () => {
    expect(formatRelativeTime(now - 2 * 3_600_000, now, t)).toBe('agent.timeHoursAgo');
    expect(formatRelativeTime(now - 23 * 3_600_000, now, t)).toBe('agent.timeHoursAgo');
  });

  it('满 24h 但不足 48h → 昨天', () => {
    expect(formatRelativeTime(now - 24 * 3_600_000, now, t)).toBe('agent.timeYesterday');
  });

  it('天数级（>=2 天）', () => {
    expect(formatRelativeTime(now - 3 * 24 * 3_600_000, now, t)).toBe('agent.timeDaysAgo');
  });

  it('非法时间戳返回空串（上层跳过显示）', () => {
    expect(formatRelativeTime(NaN, now, t)).toBe('');
    expect(formatRelativeTime(now, NaN, t)).toBe('');
  });
});
