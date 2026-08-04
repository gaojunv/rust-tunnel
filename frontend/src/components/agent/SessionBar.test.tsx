// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import SessionBar from './SessionBar';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('../../api/client', () => ({
  listAgentSessions: vi.fn().mockResolvedValue([
    { id: 's1', workspace_id: 'w1', title: '修复登录', status: 'active', created_at: '', updated_at: '' },
    { id: 's2', workspace_id: 'w1', title: null, status: 'active', created_at: '', updated_at: '' },
  ]),
  createAgentSession: vi.fn(),
  deleteAgentSession: vi.fn(),
  updateAgentSessionTitle: vi.fn(),
}));

afterEach(() => {
  cleanup();
});

const renderBar = (sessionId = 's1') => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <SessionBar
        workspaceId="w1"
        sessionId={sessionId}
        onSelect={vi.fn()}
        onDeletedCurrent={vi.fn()}
        onNew={vi.fn()}
      />
    </QueryClientProvider>
  );
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
});
