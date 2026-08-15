// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { useState } from 'react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { AgentSkill, SkillFilters } from '../../../types';
import SkillList from './SkillList';

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

const skillFixture: AgentSkill = {
  id: 's1',
  name: 'Release checklist',
  description: 'Run before every release',
  content: '', // 列表响应不含 content
  scope_type: 'global',
  client_id: '',
  workspace_id: '',
  tags: ['deploy', 'review'],
  enabled: true,
  source_session_id: 's1',
  source_trigger: 'distill',
  use_count: 3,
  last_used_at: null,
  created_at: '2026-08-01T00:00:00Z',
  updated_at: '2026-08-02T00:00:00Z',
};

const onFiltersChange = vi.fn();
const onSelect = vi.fn();
const onNew = vi.fn();

// 有状态宿主：模拟 SkillSection 持有 filters 并回写，UI 随过滤条件变化（受控组件）。
function Harness({ skills }: { skills: AgentSkill[] }) {
  const [filters, setFilters] = useState<SkillFilters>({
    scope: 'all',
    clientId: '',
    workspaceId: '',
    q: '',
    enabledOnly: false,
  });
  const change = (f: SkillFilters) => {
    onFiltersChange(f);
    setFilters(f);
  };
  return (
    <SkillList
      skills={skills}
      filters={filters}
      onFiltersChange={change}
      selectedId={null}
      onSelect={onSelect}
      onNew={onNew}
    />
  );
}

const renderList = (skills: AgentSkill[] = [skillFixture]) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <Harness skills={skills} />
    </QueryClientProvider>
  );
};

describe('SkillList', () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('renders skill cards with name/description/scope badge/enabled/tags/uses', () => {
    renderList();
    expect(screen.getByText('Release checklist')).toBeTruthy();
    expect(screen.getByText('Run before every release')).toBeTruthy();
    // scope 下拉里也有同文案的 option，卡片 Badge 用 getAllByText 断言
    expect(screen.getAllByText('skill.scope_global').length).toBeGreaterThan(0);
    expect(screen.getByText('skill.enabled')).toBeTruthy();
    expect(screen.getByText('skill.trigger_distill')).toBeTruthy();
    expect(screen.getByText('skill.uses')).toBeTruthy();
    // 标签显示
    expect(screen.getByText('deploy')).toBeTruthy();
    expect(screen.getByText('review')).toBeTruthy();
  });

  it('switching scope to client reveals client select and clears stale bindings', () => {
    renderList();
    const scope = screen.getByLabelText('skill.scopeLabel') as HTMLSelectElement;
    fireEvent.change(scope, { target: { value: 'client' } });

    expect(onFiltersChange).toHaveBeenCalledWith({
      scope: 'client',
      clientId: '',
      workspaceId: '',
      q: '',
      enabledOnly: false,
    });
    // client 下拉出现
    expect(screen.getByLabelText('skill.clientLabel')).toBeTruthy();
    expect(screen.queryByLabelText('skill.workspaceLabel')).toBeNull();
  });

  it('switching scope to workspace reveals workspace select', () => {
    renderList();
    const scope = screen.getByLabelText('skill.scopeLabel') as HTMLSelectElement;
    fireEvent.change(scope, { target: { value: 'workspace' } });
    expect(screen.getByLabelText('skill.workspaceLabel')).toBeTruthy();
  });

  it('toggling enabled filter commits enabledOnly=true', () => {
    renderList();
    fireEvent.click(screen.getByLabelText('skill.enabledOnly'));
    expect(onFiltersChange).toHaveBeenCalledWith({
      scope: 'all',
      clientId: '',
      workspaceId: '',
      q: '',
      enabledOnly: true,
    });
  });

  it('debounces search input into filters.q', async () => {
    renderList();
    fireEvent.change(screen.getByLabelText('skill.searchPlaceholder'), {
      target: { value: 'release' },
    });
    await waitFor(
      () => {
        expect(onFiltersChange).toHaveBeenCalledWith(
          expect.objectContaining({ q: 'release' }),
        );
      },
      { timeout: 1500 },
    );
  });

  it('shows empty state when there are no skills', () => {
    renderList([]);
    expect(screen.getByText('skill.empty')).toBeTruthy();
  });
});
