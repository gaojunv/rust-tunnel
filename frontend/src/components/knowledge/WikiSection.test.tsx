// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { AgentWiki } from '@/types';
import WikiSection from './WikiSection';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const api = vi.hoisted(() => ({
  listWikis: vi.fn(),
  createWiki: vi.fn(),
  updateWiki: vi.fn(),
  deleteWiki: vi.fn(),
  listAgentWorkspaces: vi.fn(),
  clientsApi: { list: vi.fn() },
}));

vi.mock('@/api/client', () => ({
  ...api,
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

// SSE 全局单例替身：不建立真实 EventSource
vi.mock('@/api/wikiStream', () => ({
  wikiStream: { subscribe: vi.fn(() => () => {}) },
}));

const wikiFixture: AgentWiki = {
  id: 'w1',
  name: 'deploy-wiki',
  summary: 'Deployment runbooks',
  status: 'ready',
  version: 2,
  page_count: 5,
  scope_type: 'workspace',
  client_id: '',
  workspace_id: 'ws1',
  created_at: '2026-08-01T00:00:00Z',
  updated_at: '2026-08-02T00:00:00Z',
};

const renderSection = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <WikiSection />
    </QueryClientProvider>,
  );
};

describe('WikiSection', () => {
  beforeEach(() => {
    api.listWikis.mockResolvedValue({ wikis: [wikiFixture], total: 1 });
    api.listAgentWorkspaces.mockResolvedValue([{ id: 'ws1', name: 'default' }]);
    api.clientsApi.list.mockResolvedValue([]);
    api.createWiki.mockResolvedValue({ ...wikiFixture, id: 'w2', name: 'ops-wiki' });
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('renders wiki container cards with name/summary/status/page count', async () => {
    renderSection();
    expect(await screen.findByText('deploy-wiki')).toBeTruthy();
    expect(screen.getByText('Deployment runbooks')).toBeTruthy();
    // status badge 与状态筛选下拉 option 同文案，用 getAllByText
    expect(screen.getAllByText('wiki.status.ready').length).toBeGreaterThan(0);
    expect(screen.getByText('wiki.pageCount')).toBeTruthy();
    // scope badge + scope 下拉 option 同文案，用 getAllByText
    expect(screen.getAllByText('wiki.scope_workspace').length).toBeGreaterThan(0);
    // 空态不出现
    expect(screen.queryByText('wiki.empty')).toBeNull();
  });

  it('shows the empty state when there are no wikis', async () => {
    api.listWikis.mockResolvedValue({ wikis: [], total: 0 });
    renderSection();
    expect(await screen.findByText('wiki.empty')).toBeTruthy();
    expect(screen.getByText('wiki.noSelection')).toBeTruthy();
  });

  it('opens the new-wiki dialog and submits scope-bound creation', async () => {
    renderSection();
    fireEvent.click(await screen.findByText('wiki.newWiki'));

    // 填 name/summary + 选 workspace
    fireEvent.change(screen.getByLabelText('wiki.name'), { target: { value: 'ops-wiki' } });
    fireEvent.change(screen.getByLabelText('wiki.summary'), { target: { value: 'Ops runbooks' } });
    const scopeSelect = screen.getByLabelText('wiki.workspaceLabel') as HTMLSelectElement;
    fireEvent.change(scopeSelect, { target: { value: 'ws1' } });

    fireEvent.click(screen.getByText('common.save'));

    await waitFor(() => {
      expect(api.createWiki).toHaveBeenCalledWith({
        name: 'ops-wiki',
        summary: 'Ops runbooks',
        scope_type: 'workspace',
        workspace_id: 'ws1',
      });
    });
  });
});
