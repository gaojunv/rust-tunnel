// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { GitChangesTab } from './ChangesTab';
import { parsePorcelainEntries } from './gitUtils';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('../../../../api/client', () => ({
  getApiErrorMessage: (err: unknown): string => {
    const data = (err as { response?: { data?: unknown } })?.response?.data;
    if (typeof data === 'string') return data;
    if (data && typeof data === 'object') {
      const msg = (data as { error?: unknown }).error;
      if (typeof msg === 'string') return msg;
    }
    return err instanceof Error ? err.message : String(err);
  },
  getAgentGitDiff: vi.fn(),
  postAgentGitStage: vi.fn(),
  postAgentGitUnstage: vi.fn(),
  postAgentGitCommit: vi.fn(),
  postAgentGitPull: vi.fn(),
  postAgentGitPush: vi.fn(),
}));

import {
  getAgentGitDiff,
  postAgentGitCommit,
  postAgentGitStage,
  postAgentGitUnstage,
} from '../../../../api/client';

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const STATUS = `## main
M  src/lib.rs
 M src/main.rs
?? notes.md
`;

const renderTab = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <GitChangesTab workspaceId="w1" entries={parsePorcelainEntries(STATUS)} branch="main...origin/main" />
    </QueryClientProvider>
  );
};

/** 行内暂存按钮：hover 才显示，测试直接按 aria-label 查找（隐藏按钮仍可被 testing-library 命中）。 */
const stageButtonFor = (path: string) => {
  const row = screen.getByText(path).closest('.group') as HTMLElement;
  return row.querySelector('button[aria-label="agent.gitStage"]') as HTMLButtonElement;
};

describe('GitChangesTab', () => {
  it('renders the three groups with entries and branch name', () => {
    renderTab();
    expect(screen.getByText('agent.stagedChanges')).toBeTruthy();
    expect(screen.getByText('agent.changes')).toBeTruthy();
    expect(screen.getByText('agent.untracked')).toBeTruthy();
    expect(screen.getByText('src/lib.rs')).toBeTruthy();
    expect(screen.getByText('src/main.rs')).toBeTruthy();
    expect(screen.getByText('notes.md')).toBeTruthy();
    // 分支名（`main...origin/main` → `main`）
    expect(screen.getByText('main')).toBeTruthy();
  });

  it('stages a single changed file on row action', async () => {
    renderTab();
    fireEvent.click(stageButtonFor('src/main.rs'));
    // mutationFn 在微任务中执行，需等待调用登记
    await waitFor(() =>
      expect(postAgentGitStage).toHaveBeenCalledWith('w1', ['src/main.rs'], false),
    );
  });

  it('unstages a staged file on row action', async () => {
    renderTab();
    const row = screen.getByText('src/lib.rs').closest('.group') as HTMLElement;
    const btn = row.querySelector('button[aria-label="agent.gitUnstage"]') as HTMLButtonElement;
    fireEvent.click(btn);
    await waitFor(() =>
      expect(postAgentGitUnstage).toHaveBeenCalledWith('w1', ['src/lib.rs'], false),
    );
  });

  it('shows approval dialog on 409 needs_approval and re-sends with approved=true', async () => {
    vi.mocked(postAgentGitStage)
      .mockRejectedValueOnce({
        response: { status: 409, data: { needs_approval: true, summary: 'git add -- src/main.rs' } },
      })
      .mockResolvedValueOnce(undefined);

    renderTab();
    fireEvent.click(stageButtonFor('src/main.rs'));

    // 审批对话框出现，显示后端摘要
    expect(await screen.findByText('git add -- src/main.rs')).toBeTruthy();

    // 确认后带 approved=true 重发
    fireEvent.click(screen.getByRole('button', { name: 'agent.approveOnce' }));
    await waitFor(() =>
      expect(postAgentGitStage).toHaveBeenLastCalledWith('w1', ['src/main.rs'], true),
    );
  });

  it('shows upgrade message when client is too old (needs_upgrade 409)', async () => {
    vi.mocked(postAgentGitStage).mockRejectedValueOnce({
      response: {
        status: 409,
        data: { needs_upgrade: true, message: 'client too old' },
      },
    });

    renderTab();
    fireEvent.click(stageButtonFor('src/main.rs'));

    expect(await screen.findByText('client too old')).toBeTruthy();
    expect(screen.queryByText('git add -- src/main.rs')).toBeNull();
  });

  it('disables commit button until a message is typed, then commits', async () => {
    renderTab();
    const commitBtn = () =>
      screen.getByRole('button', { name: 'agent.gitCommit' }) as HTMLButtonElement;

    // 空 message → 禁用
    expect(commitBtn().disabled).toBe(true);

    const textarea = screen.getByLabelText('agent.gitCommit') as HTMLTextAreaElement;
    fireEvent.change(textarea, { target: { value: 'fix: something' } });
    expect(commitBtn().disabled).toBe(false);

    fireEvent.click(commitBtn());
    await waitFor(() =>
      expect(postAgentGitCommit).toHaveBeenCalledWith('w1', 'fix: something', false),
    );
  });

  it('expands a staged file showing the cached diff', async () => {
    vi.mocked(getAgentGitDiff).mockResolvedValue('diff --git a/src/lib.rs b/src/lib.rs\n+let y = 2;\n');
    renderTab();
    fireEvent.click(screen.getByRole('button', { name: /src\/lib\.rs/ }));
    expect(await screen.findByText('+let y = 2;')).toBeTruthy();
    // staged 组 → cached=true
    expect(getAgentGitDiff).toHaveBeenCalledWith('w1', 'src/lib.rs', true);
  });
});
