// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import SkillSection from './SkillSection';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const api = vi.hoisted(() => ({
  getMemorySettings: vi.fn(),
  listSkills: vi.fn(),
  getSkill: vi.fn(),
  createSkill: vi.fn(),
  updateSkill: vi.fn(),
  deleteSkill: vi.fn(),
  toggleSkill: vi.fn(),
  listAgentWorkspaces: vi.fn(),
  clientsApi: { list: vi.fn() },
}));

vi.mock('@/api/client', () => ({
  ...api,
  listSkills: api.listSkills,
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

vi.mock('@/api/memoryStream', () => ({
  memoryStream: { subscribe: vi.fn(() => () => {}) },
}));

vi.mock('@/utils/format', () => ({
  formatDateTime: (s: string) => `fmt:${s}`,
  formatBytes: (n: number) => `${n} B`,
  formatBps: (n: number) => `${n} B/s`,
  formatMs: (n: number) => `${n} ms`,
  formatPercent: (n: number) => `${n}%`,
}));

const renderSection = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <SkillSection />
    </QueryClientProvider>,
  );
};

describe('SkillSection', () => {
  beforeEach(() => {
    api.listSkills.mockResolvedValue({ skills: [], total: 0 });
    api.getMemorySettings.mockResolvedValue({
      skill_enabled: true,
      skill_list_max: 20,
      has_key: false,
    });
    api.listAgentWorkspaces.mockResolvedValue([]);
    api.clientsApi.list.mockResolvedValue([]);
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('renders empty list state', async () => {
    renderSection();
    expect(await screen.findByText('skill.empty')).toBeTruthy();
    expect(screen.getByText('skill.noSelection')).toBeTruthy();
  });

  it('renders skill cards when the list has data', async () => {
    api.listSkills.mockResolvedValue({
      skills: [
        {
          id: 's1',
          name: 'Release checklist',
          description: 'Run before every release',
          content: '',
          scope_type: 'global',
          client_id: '',
          workspace_id: '',
          tags: ['deploy'],
          enabled: true,
          source_session_id: 's1',
          source_trigger: 'distill',
          use_count: 3,
          last_used_at: null,
          created_at: '2026-08-01T00:00:00Z',
          updated_at: '2026-08-02T00:00:00Z',
        },
      ],
      total: 1,
    });
    renderSection();
    expect(await screen.findByText('Release checklist')).toBeTruthy();
    expect(screen.getByText('skill.listTitle (1)')).toBeTruthy();
  });
});
