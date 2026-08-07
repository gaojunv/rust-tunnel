// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen, fireEvent } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import ActivityBar from './ActivityBar';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('../../api/client', () => ({
  listAgentMessages: vi.fn().mockResolvedValue([]),
}));

afterEach(() => {
  cleanup();
});

const renderBar = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ActivityBar sessionId="s1" workspaceId="w1" />
    </QueryClientProvider>
  );
};

describe('ActivityBar', () => {
  it('renders three icon buttons without text labels', () => {
    renderBar();
    expect(screen.getByRole('button', { name: 'agent.files' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'agent.terminal' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'agent.git' })).toBeTruthy();
  });

  it('expands git panel on icon click and collapses on second click', () => {
    renderBar();
    const gitBtn = screen.getByRole('button', { name: 'agent.git' });
    // 初始无面板
    expect(screen.queryByTestId('activity-panel')).toBeNull();
    // 点击展开
    fireEvent.click(gitBtn);
    expect(screen.getByTestId('activity-panel')).toBeTruthy();
    // 再点击收起
    fireEvent.click(gitBtn);
    expect(screen.queryByTestId('activity-panel')).toBeNull();
  });

  it('switches panel when clicking a different icon', () => {
    renderBar();
    fireEvent.click(screen.getByRole('button', { name: 'agent.git' }));
    expect(screen.getByTestId('activity-panel').getAttribute('data-panel')).toBe('git');
    fireEvent.click(screen.getByRole('button', { name: 'agent.files' }));
    expect(screen.getByTestId('activity-panel').getAttribute('data-panel')).toBe('files');
  });

  it('shows a resize handle only when a panel is open', () => {
    renderBar();
    expect(screen.queryByTestId('activity-panel-resizer')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'agent.files' }));
    expect(screen.getByTestId('activity-panel-resizer')).toBeTruthy();
  });

  it('resizes the panel by dragging the handle and clamps to min width', () => {
    renderBar();
    fireEvent.click(screen.getByRole('button', { name: 'agent.files' }));
    const panel = screen.getByTestId('activity-panel');
    const handle = screen.getByTestId('activity-panel-resizer');
    // 默认 288px
    expect(panel.style.width).toBe('288px');
    // 向右拖 100px → 388px
    fireEvent.pointerDown(handle, { clientX: 500 });
    fireEvent.pointerMove(window, { clientX: 600 });
    fireEvent.pointerUp(window);
    expect(panel.style.width).toBe('388px');
    // 向左拖超过最小宽度 → 钳到 200px
    fireEvent.pointerDown(handle, { clientX: 500 });
    fireEvent.pointerMove(window, { clientX: 0 });
    fireEvent.pointerUp(window);
    expect(panel.style.width).toBe('200px');
  });

  it('remembers width per panel kind', () => {
    renderBar();
    fireEvent.click(screen.getByRole('button', { name: 'agent.git' }));
    const handle = screen.getByTestId('activity-panel-resizer');
    fireEvent.pointerDown(handle, { clientX: 500 });
    fireEvent.pointerMove(window, { clientX: 560 });
    fireEvent.pointerUp(window);
    expect(screen.getByTestId('activity-panel').style.width).toBe('380px');
    // 切到 files 再切回 git，宽度保持
    fireEvent.click(screen.getByRole('button', { name: 'agent.files' }));
    expect(screen.getByTestId('activity-panel').style.width).toBe('288px');
    fireEvent.click(screen.getByRole('button', { name: 'agent.git' }));
    expect(screen.getByTestId('activity-panel').style.width).toBe('380px');
  });
});
