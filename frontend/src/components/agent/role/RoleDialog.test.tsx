// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { AgentRole } from '@/types';
import RoleDialog from './RoleDialog';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string, _o?: unknown) => k }),
}));

const api = vi.hoisted(() => ({
  createRole: vi.fn(),
  updateRole: vi.fn(),
  listAgentWorkspaces: vi.fn(),
  clientsApi: { list: vi.fn() },
  listAgentSelectableModels: vi.fn(),
}));

vi.mock('@/api/client', () => ({
  ...api,
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

vi.mock('@/api/agentModels', () => ({
  listAgentSelectableModels: api.listAgentSelectableModels,
}));

// 成功/失败 toast 已迁移到全局 MutationCache（App.tsx）按 hook meta 发出，
// 组件测试使用独立 QueryClient（无该缓存），不再断言 toast。

const roleFixture: AgentRole = {
  id: 'r1',
  name: 'code-reviewer',
  description: 'Reviews code',
  system_prompt: 'You are a reviewer',
  tools_allow: null,
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

function renderDialog(opts: { role?: AgentRole | null; onClose?: () => void }) {
  const onClose = opts.onClose ?? vi.fn();
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const r = render(
    <QueryClientProvider client={qc}>
      <RoleDialog open role={opts.role ?? null} onClose={onClose} />
    </QueryClientProvider>,
  );
  return { onClose, ...r };
}

describe('RoleDialog', () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('编辑保存成功 → onClose 被调', async () => {
    api.listAgentWorkspaces.mockResolvedValue([]);
    api.clientsApi.list.mockResolvedValue([]);
    api.listAgentSelectableModels.mockResolvedValue({ models: [], groups: [] });
    api.updateRole.mockImplementation((_id: string, _req: unknown) => Promise.resolve({ ...roleFixture }));
    // useUpdateRole 的 mutate 需真实走 tanstack mutation；mock client 层更直接
    // 但 RoleDialog 用的是 useCreateRole/useUpdateRole hooks，直接 mock @/api/client 即可
    // 为让 mutate 异步回调触发，需让 vi.hoisted 的实现返回 resolved
    const onClose = vi.fn();
    renderDialog({ role: roleFixture, onClose });
    await screen.findByDisplayValue('code-reviewer');
    fireEvent.click(screen.getByText('common.save'));
    await waitFor(() => expect(api.updateRole).toHaveBeenCalled());
    await waitFor(() => expect(onClose).toHaveBeenCalled());
  });

  it('编辑保存失败 → 内联错误且弹窗不关闭', async () => {
    api.listAgentWorkspaces.mockResolvedValue([]);
    api.clientsApi.list.mockResolvedValue([]);
    api.listAgentSelectableModels.mockResolvedValue({ models: [], groups: [] });
    api.updateRole.mockRejectedValue(new Error('boom'));
    const onClose = vi.fn();
    renderDialog({ role: roleFixture, onClose });
    await screen.findByDisplayValue('code-reviewer');
    fireEvent.click(screen.getByText('common.save'));
    await waitFor(() => expect(screen.getByText('role.saveError')).toBeTruthy());
    expect(onClose).not.toHaveBeenCalled();
  });

  it('新建保存成功 → onClose 被调', async () => {
    api.listAgentWorkspaces.mockResolvedValue([]);
    api.clientsApi.list.mockResolvedValue([]);
    api.listAgentSelectableModels.mockResolvedValue({ models: [], groups: [] });
    api.createRole.mockResolvedValue({ ...roleFixture, id: 'r-new', name: 'new-role' });
    const onClose = vi.fn();
    renderDialog({ role: null, onClose });
    // 新建：填名称即可（global 作用域无需 client/workspace）
    fireEvent.change(screen.getByLabelText('role.name'), { target: { value: 'new-role' } });
    fireEvent.click(screen.getByText('common.save'));
    await waitFor(() => expect(api.createRole).toHaveBeenCalled());
    await waitFor(() => expect(onClose).toHaveBeenCalled());
  });

  it('新建保存失败 → 内联错误且弹窗不关闭', async () => {
    api.listAgentWorkspaces.mockResolvedValue([]);
    api.clientsApi.list.mockResolvedValue([]);
    api.listAgentSelectableModels.mockResolvedValue({ models: [], groups: [] });
    api.createRole.mockRejectedValue(new Error('fail'));
    const onClose = vi.fn();
    renderDialog({ role: null, onClose });
    fireEvent.change(screen.getByLabelText('role.name'), { target: { value: 'new-role' } });
    fireEvent.click(screen.getByText('common.save'));
    await waitFor(() => expect(screen.getByText('role.saveError')).toBeTruthy());
    expect(onClose).not.toHaveBeenCalled();
  });
});
