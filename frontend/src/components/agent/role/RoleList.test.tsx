// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { useState } from 'react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { AgentRole, RoleListParams } from '../../../types';
import RoleList from './RoleList';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('../../../api/client', () => ({
  listRoles: vi.fn().mockResolvedValue({ roles: [], total: 0 }),
  toggleRole: vi.fn().mockResolvedValue({}),
  deleteRole: vi.fn().mockResolvedValue(undefined),
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

vi.mock('../../../api/hooks', () => ({
  useToggleRole: () => ({ mutate: vi.fn() }),
  useDeleteRole: () => ({ mutate: vi.fn() }),
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

const onFiltersChange = vi.fn();
const onNew = vi.fn();
const onEdit = vi.fn();

function Harness({ roles }: { roles: AgentRole[] }) {
  const [filters, setFilters] = useState<RoleListParams>({
    scope: 'all',
    q: '',
  });
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

const renderList = (roles: AgentRole[] = [roleFixture]) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <Harness roles={roles} />
    </QueryClientProvider>
  );
};

describe('RoleList', () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('renders role cards with name/description/scope/mode badges', () => {
    renderList();
    expect(screen.getByText('code-reviewer')).toBeTruthy();
    expect(screen.getByText('Reviews code changes for quality and safety')).toBeTruthy();
    expect(screen.getByText('role.scope_global')).toBeTruthy();
    expect(screen.getByText('role.mode_subagent')).toBeTruthy();
  });

  it('shows built-in badge for builtin roles', () => {
    renderList([builtinFixture]);
    expect(screen.getByText('role.builtin')).toBeTruthy();
  });

  it('shows empty state when there are no roles', () => {
    renderList([]);
    expect(screen.getByText('role.empty')).toBeTruthy();
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
});
