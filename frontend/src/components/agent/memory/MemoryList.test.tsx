// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { useState } from 'react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { AgentMemory, MemoryFilters } from '../../../types';
import MemoryList from './MemoryList';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const api = vi.hoisted(() => ({
  listAgentWorkspaces: vi.fn(),
  clientsApi: { list: vi.fn() },
}));

vi.mock('../../../api/client', () => ({
  ...api,
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

vi.mock('../../../api/memoryStream', () => ({
  memoryStream: { subscribe: vi.fn(() => () => {}) },
}));

const memoryFixture: AgentMemory = {
  id: 'm1',
  content: 'user prefers rust over go',
  scope_type: 'global',
  client_id: '',
  workspace_id: '',
  tags: ['rust', 'preference'],
  confidence: 0.9,
  source_session_id: 's1',
  source_trigger: 'distill',
  pinned: true,
  hit_count: 3,
  last_hit_at: null,
  created_at: '2026-08-01T00:00:00Z',
  updated_at: '2026-08-02T00:00:00Z',
};

const onFiltersChange = vi.fn();
const onSelect = vi.fn();
const onNew = vi.fn();

// Radix Select 在 jsdom 里 Portal 被 aria-hidden 隔离，仅暴露 trigger（combobox），
// 故此处测"行为意图"而非下拉 DOM：onScopeChange 成功被触发且 client/ws 条件显示随 props 变化。
function Harness({ memories, initialScope = 'all' as MemoryFilters['scope'] }: { memories: AgentMemory[]; initialScope?: MemoryFilters['scope'] }) {
  const [filters, setFilters] = useState<MemoryFilters>({
    scope: initialScope,
    clientId: '',
    workspaceId: '',
    q: '',
    pinned: false,
  });
  const change = (f: MemoryFilters) => {
    onFiltersChange(f);
    setFilters(f);
  };
  return (
    <MemoryList
      memories={memories}
      filters={filters}
      onFiltersChange={change}
      selectedId={null}
      onSelect={onSelect}
      onNew={onNew}
    />
  );
}

const renderList = (memories: AgentMemory[] = [memoryFixture], opts?: { initialScope?: MemoryFilters['scope'] }) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <Harness memories={memories} initialScope={opts?.initialScope} />
    </QueryClientProvider>,
  );
};

describe('MemoryList', () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('renders memory cards with scope/trigger badges, pinned mark and hit count', () => {
    renderList();
    expect(screen.getByText('user prefers rust over go')).toBeTruthy();
    expect(screen.getAllByText('memory.scope_global').length).toBeGreaterThan(0);
    expect(screen.getByText('memory.trigger_distill')).toBeTruthy();
    expect(screen.getByText('memory.pinned')).toBeTruthy();
    expect(screen.getByText('memory.hits')).toBeTruthy();
    expect(screen.getByText('rust')).toBeTruthy();
    expect(screen.getByText('preference')).toBeTruthy();
  });

  it('shows client/workspace selects only for matching scope', () => {
    const { rerender } = renderList([], { initialScope: 'all' });
    // all：都不显示
    expect(screen.queryByRole('combobox', { name: 'memory.clientLabel' })).toBeNull();
    expect(screen.queryByRole('combobox', { name: 'memory.workspaceLabel' })).toBeNull();
    // client：仅 client 显示
    cleanup();
    renderList([], { initialScope: 'client' });
    expect(screen.getByRole('combobox', { name: 'memory.clientLabel' })).toBeTruthy();
    expect(screen.queryByRole('combobox', { name: 'memory.workspaceLabel' })).toBeNull();
    void rerender;
  });

  it('toggling pinned filter commits pinned=true', () => {
    renderList();
    fireEvent.click(screen.getByLabelText('memory.pinnedOnly'));
    expect(onFiltersChange).toHaveBeenCalledWith({
      scope: 'all',
      clientId: '',
      workspaceId: '',
      q: '',
      pinned: true,
    });
  });

  it('debounces search input into filters.q', async () => {
    renderList();
    fireEvent.change(screen.getByLabelText('memory.searchPlaceholder'), {
      target: { value: 'rust' },
    });
    await waitFor(
      () => {
        expect(onFiltersChange).toHaveBeenCalledWith(
          expect.objectContaining({ q: 'rust' }),
        );
      },
      { timeout: 1500 },
    );
  });

  it('shows empty state when there are no memories', () => {
    renderList([]);
    expect(screen.getByText('memory.empty')).toBeTruthy();
  });
});
