// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { useState } from 'react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { AgentRole, RoleListParams } from '@/types';
import RoleList from './RoleList';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const toggleMutate = vi.fn();
const deleteMutate = vi.fn();

vi.mock('@/api/client', () => ({
  getApiErrorMessage: (err: unknown) => (err as { message?: string })?.message ?? String(err),
}));

vi.mock('@/api/hooks', () => ({
  useToggleRole: () => ({ mutate: toggleMutate }),
  useDeleteRole: () => ({ mutate: deleteMutate }),
}));

const roleFixture: AgentRole = {
  id: 'r1',
  name: 'code-reviewer',
  description: 'Reviews code changes for quality and safety',
  system_prompt: 'You are a code reviewer.',
  tools_allow: ['read_file', 'search', 'git_status'],
  tools_deny: null,
  model_override: null,
  mode: 'subagent',
  scope_type: 'global',
  client_id: '',
  workspace_id: '',
  is_builtin: false,
  enabled: true,
  created_at: '2026-08-01T00:00:00Z',
  updated_at: '2026-08-02T00:00:00Z',
};

const builtinFixture: AgentRole = {
  ...roleFixture,
  id: 'r2',
  name: 'general',
  description: 'Default subagent role',
  is_builtin: true,
};

const makeRoles = (n: number): AgentRole[] =>
  Array.from({ length: n }, (_, i) => ({
    ...roleFixture,
    id: `r${i}`,
    name: `role-${i}`,
    scope_type: (i % 2 === 0 ? 'global' : 'client') as AgentRole['scope_type'],
  }));

const onFiltersChange = vi.fn();
const onNew = vi.fn();
const onEdit = vi.fn();

function Harness({
  roles,
  initialFilters,
}: {
  roles: AgentRole[];
  initialFilters?: RoleListParams;
}) {
  const [filters, setFilters] = useState<RoleListParams>(
    initialFilters ?? { scope: 'all', q: '' },
  );
  const change = (f: RoleListParams) => {
    onFiltersChange(f);
    setFilters(f);
  };
  return (
    <RoleList
      roles={roles}
      filters={filters}
      onFiltersChange={change}
      onNew={onNew}
      onEdit={onEdit}
    />
  );
}

const renderList = (roles: AgentRole[] = [roleFixture], initialFilters?: RoleListParams) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <Harness roles={roles} initialFilters={initialFilters} />
    </QueryClientProvider>
  );
};

describe('RoleList', () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('renders role cards with name/scope/mode badges', () => {
    renderList();
    expect(screen.getByText('code-reviewer')).toBeTruthy();
    expect(screen.getByText('role.scope_global')).toBeTruthy();
    expect(screen.getByText('role.mode_subagent')).toBeTruthy();
  });

  it('shows built-in badge for builtin roles', () => {
    renderList([builtinFixture]);
    expect(screen.getByText('role.builtin')).toBeTruthy();
  });

  it('shows empty state when there are no roles and filter inactive', () => {
    renderList([]);
    expect(screen.getByText('role.empty')).toBeTruthy();
  });

  it('shows noSearchResults when filter active and no roles', () => {
    renderList([], { scope: 'global', q: '' });
    expect(screen.getByText('role.noSearchResults')).toBeTruthy();
    cleanup();
    renderList([], { scope: 'all', q: 'x' });
    expect(screen.getByText('role.noSearchResults')).toBeTruthy();
    cleanup();
    renderList([], { scope: 'all', q: '', enabled: true });
    expect(screen.getByText('role.noSearchResults')).toBeTruthy();
  });

  it('renders compact rows with fixed height', () => {
    renderList();
    // 紧凑行：CardContent 上用 h-12（48px）而非 p-4 大卡片
    const row = document.querySelector('.h-12');
    expect(row).toBeTruthy();
  });

  it('debounces search input into filters.q', async () => {
    renderList();
    fireEvent.change(screen.getByLabelText('role.searchPlaceholder'), {
      target: { value: 'reviewer' },
    });
    await waitFor(
      () => {
        expect(onFiltersChange).toHaveBeenCalledWith(
          expect.objectContaining({ q: 'reviewer' }),
        );
      },
      { timeout: 1500 },
    );
  });

  it('scope Select change triggers onFiltersChange', async () => {
    renderList([roleFixture]);
    const trigger = screen.getByRole('combobox', { name: 'role.scopeFilter' });
    fireEvent.click(trigger);
    const options = await screen.findAllByText('role.scope_global');
    // 最后一个是 portal 中的 SelectItem（前面的是卡片 badge）
    fireEvent.click(options[options.length - 1]);
    expect(onFiltersChange).toHaveBeenCalledWith(expect.objectContaining({ scope: 'global' }));
  });

  it('onNew is called when new button is clicked', () => {
    renderList();
    fireEvent.click(screen.getByText('role.newRole'));
    expect(onNew).toHaveBeenCalled();
  });

  it('onEdit is called when edit button is clicked', () => {
    renderList();
    const editButtons = screen.getAllByLabelText('role.editRole');
    fireEvent.click(editButtons[0]);
    expect(onEdit).toHaveBeenCalledWith(roleFixture);
  });

  it('toggle failure shows error banner', async () => {
    renderList([roleFixture]);
    const switches = screen.getAllByLabelText('role.toggle');
    fireEvent.click(switches[0]);
    const call = toggleMutate.mock.calls[0];
    expect(call).toBeTruthy();
    const opts = call[1] as { onError?: (err: unknown) => void } | undefined;
    expect(opts?.onError).toBeTruthy();
    opts!.onError!({ message: 'boom' } as unknown as Error);
    expect(await screen.findByText('role.actionFailed')).toBeTruthy();
  });

  it('delete failure shows error banner', async () => {
    renderList([roleFixture]);
    fireEvent.click(screen.getByLabelText('role.deleteRole'));
    const confirmBtns = await screen.findAllByText('common.delete');
    fireEvent.click(confirmBtns[confirmBtns.length - 1]);
    expect(deleteMutate).toHaveBeenCalled();
    const call = deleteMutate.mock.calls[0];
    const opts = call[1] as { onError?: (err: unknown) => void; onSuccess?: () => void } | undefined;
    expect(opts?.onError).toBeTruthy();
    opts!.onError!({ message: 'fail' } as unknown as Error);
    expect(await screen.findByText('role.actionFailed')).toBeTruthy();
  });

  it('load more increments visible count', () => {
    const roles = makeRoles(30);
    renderList(roles);
    // 初始 visibleCount=20，只渲染 20 行
    expect(screen.getByText('role-0')).toBeTruthy();
    expect(screen.getByText('role-19')).toBeTruthy();
    expect(screen.queryByText('role-20')).toBeNull();
    // 点击加载更多
    fireEvent.click(screen.getByText('common.loadMore'));
    expect(screen.getByText('role-20')).toBeTruthy();
    // 计数文案由 LoadMoreFooter 渲染：loaded=40 capped to total 30，total 30
    expect(screen.getByText(/common.loadedOf/)).toBeTruthy();
  });
});
