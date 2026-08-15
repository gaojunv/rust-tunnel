// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { AgentMemory } from '../../../types';
import MemoryDialog from './MemoryDialog';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const api = vi.hoisted(() => ({
  createMemory: vi.fn(),
  updateMemory: vi.fn(),
  listAgentWorkspaces: vi.fn(),
  clientsApi: { list: vi.fn() },
}));

vi.mock('../../../api/client', () => ({
  ...api,
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

const memoryFixture: AgentMemory = {
  id: 'm1',
  content: 'user prefers rust over go',
  scope_type: 'global',
  client_id: '',
  workspace_id: '',
  tags: ['rust'],
  confidence: 0.9,
  source_session_id: 's1',
  source_trigger: 'distill',
  pinned: true,
  hit_count: 3,
  last_hit_at: null,
  created_at: '2026-08-01T00:00:00Z',
  updated_at: '2026-08-02T00:00:00Z',
};

const renderDialog = (memory?: AgentMemory | null) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryDialog
        open
        onClose={vi.fn()}
        memory={memory ?? null}
        onCreated={vi.fn()}
      />
    </QueryClientProvider>
  );
};

describe('MemoryDialog', () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('disables save while content is empty', () => {
    api.listAgentWorkspaces.mockResolvedValue([]);
    api.clientsApi.list.mockResolvedValue([]);
    renderDialog();
    const save = screen.getByText('common.save') as HTMLButtonElement;
    expect(save.disabled).toBe(true);
  });

  it('creates a workspace-scoped memory with binding', async () => {
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
    api.createMemory.mockResolvedValue({ ...memoryFixture, id: 'm-new' });

    renderDialog();
    await screen.findByRole('option', { name: 'proj' });

    fireEvent.change(screen.getByLabelText('memory.content'), { target: { value: 'new fact' } });
    fireEvent.change(screen.getByLabelText('memory.workspaceLabel'), { target: { value: 'w1' } });
    fireEvent.click(screen.getByText('common.save'));

    await waitFor(() => {
      expect(api.createMemory).toHaveBeenCalledWith({
        content: 'new fact',
        scope: 'workspace',
        workspace_id: 'w1',
        tags: [],
        confidence: 0.8,
      });
    });
  });

  it('creates a client-scoped memory with client binding', async () => {
    api.listAgentWorkspaces.mockResolvedValue([]);
    api.clientsApi.list.mockResolvedValue([{ name: 'nas', online: true }]);
    api.createMemory.mockResolvedValue({ ...memoryFixture, id: 'm-new' });

    renderDialog();
    // 先切到 client 作用域，客户端下拉才会出现（默认 workspace）
    fireEvent.change(screen.getByLabelText('memory.scopeLabel'), { target: { value: 'client' } });
    await screen.findByRole('option', { name: 'nas' });

    fireEvent.change(screen.getByLabelText('memory.content'), {
      target: { value: 'client fact' },
    });
    fireEvent.change(screen.getByLabelText('memory.clientLabel'), { target: { value: 'nas' } });
    fireEvent.click(screen.getByText('common.save'));

    await waitFor(() => {
      expect(api.createMemory).toHaveBeenCalledWith({
        content: 'client fact',
        scope: 'client',
        client_id: 'nas',
        tags: [],
        confidence: 0.8,
      });
    });
  });

  it('edit mode prefills fields and updates via PUT', async () => {
    api.listAgentWorkspaces.mockResolvedValue([]);
    api.clientsApi.list.mockResolvedValue([]);
    api.updateMemory.mockResolvedValue(memoryFixture);

    renderDialog(memoryFixture);
    await screen.findByDisplayValue('user prefers rust over go');

    fireEvent.change(screen.getByLabelText('memory.content'), {
      target: { value: 'user prefers rust, not go' },
    });
    fireEvent.click(screen.getByText('common.save'));

    await waitFor(() => {
      expect(api.updateMemory).toHaveBeenCalledWith('m1', {
        content: 'user prefers rust, not go',
        tags: ['rust'],
        scope: 'global',
        confidence: 0.9,
      });
    });
  });
});
