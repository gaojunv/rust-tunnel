// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import GitHubActionsPanel from './GitHubActionsPanel';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const api = vi.hoisted(() => ({
  getAgentGithubRepo: vi.fn(),
  getAgentGithubWorkflows: vi.fn(),
  getAgentGithubRuns: vi.fn(),
  getAgentGithubRunJobs: vi.fn(),
  getAgentGithubJobLogs: vi.fn(),
  postAgentGithubDispatch: vi.fn(),
  postAgentGithubRerun: vi.fn(),
  postAgentGithubCancel: vi.fn(),
}));

vi.mock('../../../../api/client', () => ({
  ...api,
  getApiErrorMessage: (err: unknown): string => {
    const data = (err as { response?: { data?: unknown } })?.response?.data;
    if (typeof data === 'string') return data;
    if (data && typeof data === 'object') {
      const msg = (data as { error?: unknown }).error;
      if (typeof msg === 'string') return msg;
    }
    return err instanceof Error ? err.message : String(err);
  },
}));

const makeClient = () =>
  new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });

const renderPanel = () => {
  const qc = makeClient();
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={qc}>{children}</QueryClientProvider>
  );
  return render(<GitHubActionsPanel workspaceId="w1" />, { wrapper });
};

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe('GitHubActionsPanel 空态', () => {
  it('token 未配置 → 引导文案，且不拉取 runs/workflows', async () => {
    api.getAgentGithubRepo.mockResolvedValue({
      configured: true,
      owner: 'octo',
      repo: 'repo',
      token_set: false,
      repo_info: null,
    });
    renderPanel();
    expect(await screen.findByText('agent.githubNoToken')).toBeTruthy();
    expect(api.getAgentGithubRuns).not.toHaveBeenCalled();
    expect(api.getAgentGithubWorkflows).not.toHaveBeenCalled();
  });

  it('token 已配置但仓库未定位 → 提示手填，刷新按钮强制重探（refresh=true）', async () => {
    api.getAgentGithubRepo.mockResolvedValue({
      configured: false,
      owner: null,
      repo: null,
      token_set: true,
      repo_info: null,
    });
    renderPanel();
    expect(await screen.findByText('agent.githubNoRepo')).toBeTruthy();
    // 第二次点击刷新后仓库可探测到 → repo 检测用 refresh=true 重新请求
    api.getAgentGithubRepo.mockResolvedValue({
      configured: true,
      owner: 'octo',
      repo: 'repo',
      token_set: true,
      repo_info: { default_branch: 'main' },
    });
    fireEvent.click(screen.getByRole('button', { name: 'agent.refresh' }));
    await waitFor(() => expect(api.getAgentGithubRepo).toHaveBeenLastCalledWith('w1', true));
  });
});

describe('GitHubActionsPanel Runs tab', () => {
  const repoState = {
    configured: true,
    owner: 'octo',
    repo: 'repo',
    token_set: true,
    repo_info: { default_branch: 'main' },
  };

  it('rerun 走 409 审批流：首次不带 approved，确认后带 approved=true 重发', async () => {
    api.getAgentGithubRepo.mockResolvedValue(repoState);
    api.getAgentGithubWorkflows.mockResolvedValue({ total_count: 0, workflows: [] });
    api.getAgentGithubRuns.mockResolvedValue({
      total_count: 1,
      workflow_runs: [
        {
          id: 123,
          name: 'CI',
          display_title: 'Build',
          head_branch: 'main',
          status: 'completed',
          conclusion: 'success',
          run_started_at: '2026-08-14T09:00:00Z',
          html_url: 'https://github.com/octo/repo/actions/runs/123',
        },
      ],
    });
    api.postAgentGithubRerun
      .mockRejectedValueOnce({
        response: { status: 409, data: { needs_approval: true, summary: 'github rerun: octo/repo runs/123' } },
      })
      .mockResolvedValueOnce({ status: 'rerun_queued' });

    renderPanel();
    await screen.findByText('Build');

    fireEvent.click(screen.getByRole('button', { name: /agent\.githubRerun/ }));
    // 审批对话框出现，summary 为后端摘要
    expect(await screen.findByText('agent.approvalRequired')).toBeTruthy();
    expect(screen.getByText('github rerun: octo/repo runs/123')).toBeTruthy();
    expect(api.postAgentGithubRerun).toHaveBeenLastCalledWith('w1', '123', false);

    // 确认后带 approved=true 重发
    fireEvent.click(screen.getByText('agent.approveOnce'));
    await waitFor(() => expect(api.postAgentGithubRerun).toHaveBeenCalledTimes(2));
    expect(api.postAgentGithubRerun).toHaveBeenLastCalledWith('w1', '123', true);
  });

  it('进行中的 run 显示 cancel 按钮，取消同样走审批流', async () => {
    api.getAgentGithubRepo.mockResolvedValue(repoState);
    api.getAgentGithubWorkflows.mockResolvedValue({ total_count: 0, workflows: [] });
    api.getAgentGithubRuns.mockResolvedValue({
      total_count: 1,
      workflow_runs: [
        {
          id: 9,
          name: 'CI',
          display_title: 'Deploy',
          head_branch: 'main',
          status: 'in_progress',
          conclusion: null,
          run_started_at: '2026-08-14T09:00:00Z',
        },
      ],
    });
    api.postAgentGithubCancel
      .mockRejectedValueOnce({
        response: { status: 409, data: { needs_approval: true, summary: 'github cancel: octo/repo runs/9' } },
      })
      .mockResolvedValueOnce({ status: 'cancel_requested' });

    renderPanel();
    await screen.findByText('Deploy');

    fireEvent.click(screen.getByRole('button', { name: /agent\.githubCancel/ }));
    expect(await screen.findByText('agent.approvalRequired')).toBeTruthy();
    fireEvent.click(screen.getByText('agent.approveOnce'));
    await waitFor(() => expect(api.postAgentGithubCancel).toHaveBeenCalledTimes(2));
    expect(api.postAgentGithubCancel).toHaveBeenLastCalledWith('w1', '9', true);
  });
});

describe('GitHubActionsPanel Workflows tab', () => {
  it('dispatch 触发对话框：ref 默认 main，确认后带 approved=true 重发', async () => {
    api.getAgentGithubRepo.mockResolvedValue({
      configured: true,
      owner: 'octo',
      repo: 'repo',
      token_set: true,
      repo_info: { default_branch: 'main' },
    });
    api.getAgentGithubWorkflows.mockResolvedValue({
      total_count: 1,
      workflows: [
        { id: 1, name: 'CI', path: '.github/workflows/ci.yml', state: 'active' },
      ],
    });
    api.getAgentGithubRuns.mockResolvedValue({ total_count: 0, workflow_runs: [] });
    api.postAgentGithubDispatch
      .mockRejectedValueOnce({
        response: {
          status: 409,
          data: { needs_approval: true, summary: 'github workflow_dispatch: octo/repo workflows/1 (ref=main)' },
        },
      })
      .mockResolvedValueOnce({ status: 'dispatched' });

    renderPanel();
    // 等面板进入正常态（tab 栏出现）后切到 Workflows tab
    const workflowsTab = await screen.findByRole('tab', { name: 'agent.githubTabWorkflows' });
    fireEvent.click(workflowsTab);
    await screen.findByText('CI');

    // 打开 dispatch 对话框 → ref 默认 main
    fireEvent.click(screen.getByRole('button', { name: /agent\.githubDispatch/ }));
    const dialog = await screen.findByRole('dialog');
    // 标题为「agent.githubDispatchTitle · CI」（拼接文本），用子串匹配
    expect(within(dialog).getByText(/agent\.githubDispatchTitle/)).toBeTruthy();
    const refInput = within(dialog).getByPlaceholderText('agent.githubRefPlaceholder') as HTMLInputElement;
    expect(refInput.value).toBe('main');

    // 触发 → 409 审批 → 确认重发
    fireEvent.click(within(dialog).getByText('agent.githubDispatch'));
    expect(await screen.findByText('agent.approvalRequired')).toBeTruthy();
    fireEvent.click(screen.getByText('agent.approveOnce'));
    await waitFor(() => expect(api.postAgentGithubDispatch).toHaveBeenCalledTimes(2));
    expect(api.postAgentGithubDispatch).toHaveBeenLastCalledWith('w1', '1', 'main', {}, true);
  });
});
