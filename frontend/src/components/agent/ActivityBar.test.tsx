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
});
