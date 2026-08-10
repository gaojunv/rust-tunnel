// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import WorkspaceBar from './WorkspaceBar';
import type { AgentWorkspace } from '../../types';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const api = vi.hoisted(() => ({
  deleteAgentWorkspace: vi.fn(),
}));

vi.mock('../../api/client', () => ({
  listAgentWorkspaces: vi.fn().mockResolvedValue([
    {
      id: 'w1',
      name: 'proj',
      client_id: 'nas',
      runtime_type: 'host',
      root_path: '/p',
      approval_mode: 'safe',
      system_prompt: null,
      created_at: '',
      updated_at: '',
    },
    {
      id: 'w2',
      name: 'proj',
      client_id: 'laptop',
      runtime_type: 'host',
      root_path: '/q',
      approval_mode: 'safe',
      system_prompt: null,
      created_at: '',
      updated_at: '',
    },
  ] satisfies AgentWorkspace[]),
  deleteAgentWorkspace: api.deleteAgentWorkspace,
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const renderBar = (workspaceId = 'w1', onSelect = vi.fn()) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const utils = render(
    <QueryClientProvider client={qc}>
      <WorkspaceBar
        workspaceId={workspaceId}
        onSelect={onSelect}
        onNew={vi.fn()}
        onEdit={vi.fn()}
      />
    </QueryClientProvider>,
  );
  return { onSelect, ...utils };
};

describe('WorkspaceBar', () => {
  it('shows current workspace name on trigger', async () => {
    renderBar('w1');
    // shadcn Select 触发器回显当前工作区名
    await waitFor(() => {
      expect(screen.getByLabelText('agent.selectWorkspaceAria').textContent).toContain('proj');
    });
  });

  it('opens confirm dialog on delete click; confirming calls delete API', async () => {
    const { onSelect } = renderBar('w1');
    api.deleteAgentWorkspace.mockResolvedValue(undefined);

    // 顶栏不直接删除：先开确认 Dialog
    fireEvent.click(screen.getByLabelText('agent.deleteWorkspace'));
    const dialog = await screen.findByRole('dialog');
    expect(dialog.textContent).toContain('agent.confirmDeleteWorkspace');
    // Dialog 描述显示工作区名称，帮助用户确认目标（等待 workspaces 查询加载）
    await waitFor(() => {
      expect(dialog.textContent).toContain('proj');
    });
    expect(api.deleteAgentWorkspace).not.toHaveBeenCalled();

    fireEvent.click(screen.getByText('agent.confirm'));
    await waitFor(() => {
      expect(api.deleteAgentWorkspace).toHaveBeenCalledWith('w1');
    });
    // 删除成功后清空选中回引导态
    await waitFor(() => {
      expect(onSelect).toHaveBeenCalledWith('');
    });
  });

  it('cancel closes the dialog without deleting', async () => {
    renderBar('w1');
    fireEvent.click(screen.getByLabelText('agent.deleteWorkspace'));
    await screen.findByRole('dialog');

    fireEvent.click(screen.getByText('agent.cancel'));
    await waitFor(() => {
      expect(screen.queryByRole('dialog')).toBeNull();
    });
    expect(api.deleteAgentWorkspace).not.toHaveBeenCalled();
  });

  it('delete failure shows error inside the dialog and keeps it open', async () => {
    renderBar('w1');
    api.deleteAgentWorkspace.mockRejectedValue(new Error('boom'));

    fireEvent.click(screen.getByLabelText('agent.deleteWorkspace'));
    await screen.findByRole('dialog');
    fireEvent.click(screen.getByText('agent.confirm'));

    // 错误显示在 Dialog 内（role=alert），弹窗保持打开供重试/取消
    expect(await screen.findByRole('alert')).toBeTruthy();
    expect(screen.getByRole('dialog')).toBeTruthy();
  });

  it('edit button is disabled when no workspace selected', () => {
    renderBar('');
    expect((screen.getByLabelText('agent.editWorkspace') as HTMLButtonElement).disabled).toBe(
      true,
    );
    // 未选中时无删除入口
    expect(screen.queryByLabelText('agent.deleteWorkspace')).toBeNull();
  });
});
