// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi, type Mock } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import SessionTabBar from './SessionTabBar';
import { MAX_TABS } from './tabsStore';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const api = vi.hoisted(() => ({
  listAgentSessions: vi.fn(),
  deleteAgentSession: vi.fn(),
  exportAgentSession: vi.fn(),
  updateAgentSessionTitle: vi.fn(),
  getApiErrorMessage: vi.fn((e: unknown) => String(e)),
}));

vi.mock('../../api/client', () => ({
  listAgentSessions: api.listAgentSessions,
  deleteAgentSession: api.deleteAgentSession,
  exportAgentSession: api.exportAgentSession,
  updateAgentSessionTitle: api.updateAgentSessionTitle,
  getApiErrorMessage: api.getApiErrorMessage,
}));

const sessions = [
  {
    id: 's1',
    workspace_id: 'w1',
    title: '修复登录',
    status: 'active',
    created_at: '2026-08-04T00:00:00Z',
    updated_at: '',
  },
  {
    id: 's2',
    workspace_id: 'w1',
    title: null,
    status: 'active',
    created_at: '2026-08-04T00:00:00Z',
    updated_at: '',
  },
];

beforeEach(() => {
  api.listAgentSessions.mockResolvedValue(sessions);
  // 默认按桌面端（matchMedia.matches=true）渲染多标签分支；个别用例可改 mobileMatches
  mockMatchMedia(true);
});

// 可控的 matchMedia mock：jsdom 无 matchMedia，useMediaQuery 会恒返回 false（移动端分支）。
// 多数断言针对桌面端多标签行为，故默认返回 true。
function mockMatchMedia(matches: boolean) {
  window.matchMedia = vi.fn().mockImplementation((query: string) => ({
    matches,
    media: query,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    onchange: null,
    dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia;
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const renderBar = ({
  open,
  active,
  onSelect = vi.fn<(id: string) => void>(),
  onClose = vi.fn<(id: string) => void>(),
  onNew = vi.fn<() => void>(),
}: {
  open: string[];
  active: string;
  onSelect?: Mock<(id: string) => void>;
  onClose?: Mock<(id: string) => void>;
  onNew?: Mock<() => void>;
}) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return {
    onSelect,
    onClose,
    onNew,
    ...render(
      <QueryClientProvider client={qc}>
        <SessionTabBar
          workspaceId="w1"
          open={open}
          active={active}
          onSelect={onSelect}
          onClose={onClose}
          onNew={onNew}
        />
      </QueryClientProvider>,
    ),
  };
};

describe('SessionTabBar', () => {
  it('renders session titles with unnamed fallback', async () => {
    renderBar({ open: ['s1', 's2'], active: 's1' });
    expect(await screen.findByText('修复登录')).toBeTruthy();
    expect(screen.getByText('agent.unnamedSession')).toBeTruthy();
  });

  it('falls back to unnamed placeholder while a session is not loaded yet', async () => {
    renderBar({ open: ['s-missing'], active: 's-missing' });
    expect(await screen.findByText('agent.unnamedSession')).toBeTruthy();
  });

  it('marks only the active tab with aria-selected', async () => {
    renderBar({ open: ['s1', 's2'], active: 's1' });
    await screen.findByText('修复登录');
    expect(screen.getByRole('tab', { name: '修复登录' }).getAttribute('aria-selected')).toBe('true');
    expect(
      screen.getByRole('tab', { name: 'agent.unnamedSession' }).getAttribute('aria-selected'),
    ).toBe('false');
  });

  it('invokes onSelect when a tab is clicked', async () => {
    const { onSelect } = renderBar({ open: ['s1', 's2'], active: 's1' });
    await screen.findByText('agent.unnamedSession');
    fireEvent.click(screen.getByRole('tab', { name: 'agent.unnamedSession' }));
    expect(onSelect).toHaveBeenCalledWith('s2');
  });

  it('close button invokes onClose but not onSelect (stopPropagation)', async () => {
    const { onSelect, onClose } = renderBar({ open: ['s1', 's2'], active: 's1' });
    await screen.findByText('修复登录');
    const closeButtons = screen.getAllByLabelText('agent.closeTab');
    fireEvent.click(closeButtons[0]); // s1
    expect(onClose).toHaveBeenCalledWith('s1');
    expect(onSelect).not.toHaveBeenCalled();
  });

  it('invokes onNew via the trailing + button', async () => {
    const { onNew } = renderBar({ open: ['s1'], active: 's1' });
    await screen.findByText('修复登录');
    fireEvent.click(screen.getByLabelText('agent.newTab'));
    expect(onNew).toHaveBeenCalled();
  });

  it('disables the + button when the tab limit is reached', async () => {
    const open = Array.from({ length: MAX_TABS }, (_, i) => `s${i}`);
    const { onNew } = renderBar({ open, active: 's0' });
    await screen.findAllByText('agent.unnamedSession');
    const newBtn = screen.getByLabelText('agent.newTab') as HTMLButtonElement;
    expect(newBtn.disabled).toBe(true);
    fireEvent.click(newBtn);
    expect(onNew).not.toHaveBeenCalled();
  });

  it('enables the + button below the limit', async () => {
    const open = Array.from({ length: MAX_TABS - 1 }, (_, i) => `s${i}`);
    const { onNew } = renderBar({ open, active: 's0' });
    await screen.findAllByText('agent.unnamedSession');
    expect((screen.getByLabelText('agent.newTab') as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(screen.getByLabelText('agent.newTab'));
    expect(onNew).toHaveBeenCalled();
  });

  it('mobile (<768px): title-only trigger opens session dropdown, no + / no other tabs', async () => {
    mockMatchMedia(false); // 切到移动端分支
    renderBar({ open: ['s1', 's2'], active: 's2' });
    // 仅当前激活会话 s2（未命名 → fallback）显示；s1 标题不出现
    expect(await screen.findByText('agent.unnamedSession')).toBeTruthy();
    expect(screen.queryByText('修复登录')).toBeNull();
    // 无独立 + 新建按钮（新建走下拉内入口）
    expect(screen.queryByLabelText('agent.newTab')).toBeNull();
    // 无 tab 角色（不是标签页形态）
    expect(screen.queryByRole('tab')).toBeNull();

    // 标题即触发器：点击打开 SessionBar 同一会话下拉（Radix 需 pointerDown 打开）
    const trigger = screen.getByLabelText('agent.selectSessionAria');
    fireEvent.pointerDown(trigger);
    fireEvent.click(trigger);
    expect(await screen.findByText('agent.newSession')).toBeTruthy();
  });
});
