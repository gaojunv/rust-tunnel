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

// 有状态宿主：模拟 MemoryPage 持有 filters 并回写，UI 随过滤条件变化（受控组件）。
function Harness({ memories }: { memories: AgentMemory[] }) {
  const [filters, setFilters] = useState<MemoryFilters>({
    scope: 'all',
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

const renderList = (memories: AgentMemory[] = [memoryFixture]) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <Harness memories={memories} />
    </QueryClientProvider>
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
    // scope 下拉里也有同文案的 option，卡片 Badge 用 getAllByText 断言
    expect(screen.getAllByText('memory.scope_global').length).toBeGreaterThan(0);
    expect(screen.getByText('memory.trigger_distill')).toBeTruthy();
    expect(screen.getByText('memory.pinned')).toBeTruthy();
    expect(screen.getByText('memory.hits')).toBeTruthy();
    // 标签显示
    expect(screen.getByText('rust')).toBeTruthy();
    expect(screen.getByText('preference')).toBeTruthy();
  });

  it('switching scope to client reveals client select and clears stale bindings', () => {
    renderList();
    const scope = screen.getByLabelText('memory.scopeLabel') as HTMLSelectElement;
    fireEvent.change(scope, { target: { value: 'client' } });

    expect(onFiltersChange).toHaveBeenCalledWith({
      scope: 'client',
      clientId: '',
      workspaceId: '',
      q: '',
      pinned: false,
    });
    // client 下拉出现
    expect(screen.getByLabelText('memory.clientLabel')).toBeTruthy();
    expect(screen.queryByLabelText('memory.workspaceLabel')).toBeNull();
  });

  it('switching scope to workspace reveals workspace select', () => {
    renderList();
    const scope = screen.getByLabelText('memory.scopeLabel') as HTMLSelectElement;
    fireEvent.change(scope, { target: { value: 'workspace' } });
    expect(screen.getByLabelText('memory.workspaceLabel')).toBeTruthy();
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
