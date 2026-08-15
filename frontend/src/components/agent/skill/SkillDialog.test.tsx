// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { AgentSkill } from '../../../types';
import SkillDialog from './SkillDialog';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const api = vi.hoisted(() => ({
  createSkill: vi.fn(),
  updateSkill: vi.fn(),
  listAgentWorkspaces: vi.fn(),
  clientsApi: { list: vi.fn() },
}));

vi.mock('../../../api/client', () => ({
  ...api,
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

const skillFixture: AgentSkill = {
  id: 's1',
  name: 'Release checklist',
  description: 'Run before every release',
  content: '1. run tests\n2. tag version',
  scope_type: 'global',
  client_id: '',
  workspace_id: '',
  tags: ['deploy'],
  enabled: true,
  source_session_id: 's1',
  source_trigger: 'manual',
  use_count: 0,
  last_used_at: null,
  created_at: '2026-08-01T00:00:00Z',
  updated_at: '2026-08-02T00:00:00Z',
};

const renderDialog = (skill?: AgentSkill | null) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <SkillDialog
        open
        onClose={vi.fn()}
        skill={skill ?? null}
        onCreated={vi.fn()}
      />
    </QueryClientProvider>
  );
};

describe('SkillDialog', () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('disables save while name or content is empty', () => {
    api.listAgentWorkspaces.mockResolvedValue([]);
    api.clientsApi.list.mockResolvedValue([]);
    renderDialog();
    const save = screen.getByText('common.save') as HTMLButtonElement;
    expect(save.disabled).toBe(true);

    // 仅填名称、内容仍空 → 依旧禁用
    fireEvent.change(screen.getByLabelText('skill.name'), { target: { value: 'release' } });
    expect((screen.getByText('common.save') as HTMLButtonElement).disabled).toBe(true);

    // 默认作用域是 workspace（还需绑定工作区）；切到 global 后填内容即可保存
    fireEvent.change(screen.getByLabelText('skill.scopeLabel'), { target: { value: 'global' } });
    fireEvent.change(screen.getByLabelText('skill.content'), { target: { value: 'steps' } });
    expect((screen.getByText('common.save') as HTMLButtonElement).disabled).toBe(false);
  });

  it('creates a workspace-scoped skill with binding', async () => {
    api.listAgentWorkspaces.mockResolvedValue([
      {
        id: 'w1',
        name: 'proj',
        client_id: 'nas',
        runtime_type: 'host',
        root_path: '/p',
        created_at: '',
        updated_at: '',
      },
    ]);
    api.clientsApi.list.mockResolvedValue([]);
    api.createSkill.mockResolvedValue({ ...skillFixture, id: 's-new' });

    renderDialog();
    await screen.findByRole('option', { name: 'proj' });

    fireEvent.change(screen.getByLabelText('skill.name'), { target: { value: 'check deploy' } });
    fireEvent.change(screen.getByLabelText('skill.content'), {
      target: { value: 'verify services up' },
    });
    fireEvent.change(screen.getByLabelText('skill.workspaceLabel'), { target: { value: 'w1' } });
    fireEvent.click(screen.getByText('common.save'));

    await waitFor(() => {
      expect(api.createSkill).toHaveBeenCalledWith({
        name: 'check deploy',
        description: '',
        content: 'verify services up',
        scope_type: 'workspace',
        workspace_id: 'w1',
        tags: [],
      });
    });
  });

  it('creates a client-scoped skill with client binding', async () => {
    api.listAgentWorkspaces.mockResolvedValue([]);
    api.clientsApi.list.mockResolvedValue([{ name: 'nas', online: true }]);
    api.createSkill.mockResolvedValue({ ...skillFixture, id: 's-new' });

    renderDialog();
    // 先切到 client 作用域，客户端下拉才会出现（默认 workspace）
    fireEvent.change(screen.getByLabelText('skill.scopeLabel'), { target: { value: 'client' } });
    await screen.findByRole('option', { name: 'nas' });

    fireEvent.change(screen.getByLabelText('skill.name'), { target: { value: 'nas runbook' } });
    fireEvent.change(screen.getByLabelText('skill.content'), {
      target: { value: 'restart service x' },
    });
    fireEvent.change(screen.getByLabelText('skill.clientLabel'), { target: { value: 'nas' } });
    fireEvent.click(screen.getByText('common.save'));

    await waitFor(() => {
      expect(api.createSkill).toHaveBeenCalledWith({
        name: 'nas runbook',
        description: '',
        content: 'restart service x',
        scope_type: 'client',
        client_id: 'nas',
        tags: [],
      });
    });
  });

  it('edit mode prefills fields and updates via PUT', async () => {
    api.listAgentWorkspaces.mockResolvedValue([]);
    api.clientsApi.list.mockResolvedValue([]);
    api.updateSkill.mockResolvedValue(skillFixture);

    renderDialog(skillFixture);
    await screen.findByDisplayValue('Release checklist');

    fireEvent.change(screen.getByLabelText('skill.name'), {
      target: { value: 'Release checklist v2' },
    });
    fireEvent.change(screen.getByLabelText('skill.description'), {
      target: { value: 'Run before every release and rollback' },
    });
    fireEvent.click(screen.getByText('common.save'));

    await waitFor(() => {
      expect(api.updateSkill).toHaveBeenCalledWith('s1', {
        name: 'Release checklist v2',
        description: 'Run before every release and rollback',
        content: '1. run tests\n2. tag version',
        scope_type: 'global',
        tags: ['deploy'],
      });
    });
  });
});
