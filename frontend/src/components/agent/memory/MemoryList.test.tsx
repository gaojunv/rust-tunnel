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

vi.mock('@/utils/format', () => ({
  formatDateTime: (s: string) => `fmt:${s}`,
  formatBytes: (n: number) => `${n} B`,
  formatBps: (n: number) => `${n} B/s`,
  formatMs: (n: number) => `${n} ms`,
  formatPercent: (n: number) => `${n}%`,
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

function Harness({ memories, total, hasMore, initialScope = 'all' as MemoryFilters['scope'] }: { memories: AgentMemory[]; total?: number; hasMore?: boolean; initialScope?: MemoryFilters['scope'] }) {
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
      total={total}
      filters={filters}
      onFiltersChange={change}
      selectedId={null}
      onSelect={onSelect}
      onNew={onNew}
      hasMore={hasMore}
      onLoadMore={hasMore ? vi.fn() : undefined}
    />
  );
}

const renderList = (memories: AgentMemory[] = [memoryFixture], opts?: { initialScope?: MemoryFilters['scope']; filters?: MemoryFilters; total?: number; hasMore?: boolean }) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  if (opts?.filters) {
    return render(
      <QueryClientProvider client={qc}>
        <MemoryList
          memories={memories}
          total={opts.total}
          filters={opts.filters}
          onFiltersChange={onFiltersChange}
          selectedId={null}
          onSelect={onSelect}
          onNew={onNew}
          hasMore={opts.hasMore}
          onLoadMore={opts.hasMore ? vi.fn() : undefined}
        />
      </QueryClientProvider>,
    );
  }
  return render(
    <QueryClientProvider client={qc}>
      <Harness memories={memories} total={opts?.total} hasMore={opts?.hasMore} initialScope={opts?.initialScope} />
    </QueryClientProvider>,
  );
};

describe('MemoryList', () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('紧凑行渲染：内容/徽章/时间同行，带 LoadMoreFooter', () => {
    const onLoadMore = vi.fn();
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <MemoryList
          memories={[memoryFixture]}
          total={5}
          filters={{ scope: 'all', clientId: '', workspaceId: '', q: '', pinned: false }}
          onFiltersChange={onFiltersChange}
          selectedId={null}
          onSelect={onSelect}
          onNew={onNew}
          hasMore
          onLoadMore={onLoadMore}
        />
      </QueryClientProvider>,
    );
    expect(screen.getByText('user prefers rust over go')).toBeTruthy();
    // 紧凑行：时间戳走 formatDateTime
    expect(screen.getByText('fmt:2026-08-02T00:00:00Z')).toBeTruthy();
    // LoadMoreFooter 计数
    expect(screen.getByText('common.loadedOf')).toBeTruthy();
    expect(screen.getByText('common.loadMore')).toBeTruthy();
    fireEvent.click(screen.getByText('common.loadMore'));
    expect(onLoadMore).toHaveBeenCalled();
  });

  it('空态区分：无过滤时显示 empty，有搜索时显示 noSearchResults', () => {
    // 无过滤 → empty
    renderList([], { filters: { scope: 'all', clientId: '', workspaceId: '', q: '', pinned: false } });
    expect(screen.getByText('memory.empty')).toBeTruthy();
    cleanup();
    // 有 q → noSearchResults
    renderList([], { filters: { scope: 'all', clientId: '', workspaceId: '', q: 'rust', pinned: false } });
    expect(screen.getByText('memory.noSearchResults')).toBeTruthy();
    cleanup();
    // pinned 也算激活过滤
    renderList([], { filters: { scope: 'all', clientId: '', workspaceId: '', q: '', pinned: true } });
    expect(screen.getByText('memory.noSearchResults')).toBeTruthy();
  });

  it('shows client/workspace selects only for matching scope', () => {
    const { rerender } = renderList([], { initialScope: 'all' });
    expect(screen.queryByRole('combobox', { name: 'memory.clientLabel' })).toBeNull();
    expect(screen.queryByRole('combobox', { name: 'memory.workspaceLabel' })).toBeNull();
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

  it('hasMore=false 时不渲染 LoadMoreFooter', () => {
    // 不传 hasMore/onLoadMore 时不渲染 footer
    renderList([memoryFixture]);
    expect(screen.queryByText('common.loadMore')).toBeNull();
  });
});
