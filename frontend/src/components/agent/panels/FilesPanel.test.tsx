// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import FilesPanel, { parsePorcelain } from './FilesPanel';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('shiki', () => ({
  codeToHtml: vi.fn().mockResolvedValue('<pre data-testid="highlight">highlighted-code</pre>'),
}));

vi.mock('../../../api/client', () => ({
  getFsTree: vi.fn(),
  getFsFile: vi.fn(),
  putFsFile: vi.fn(),
  getAgentGitStatus: vi.fn(),
  getApiErrorMessage: vi.fn((err: unknown) => String(err)),
}));

import { getAgentGitStatus, getFsFile, getFsTree } from '../../../api/client';

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const renderPanel = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <FilesPanel workspaceId="w1" />
    </QueryClientProvider>
  );
};

describe('parsePorcelain', () => {
  it('skips branch header and normalizes index/worktree statuses', () => {
    const map = parsePorcelain(`## main...origin/main
M  src/lib.rs
 M src/main.rs
`);
    expect(map.get('src/lib.rs')).toBe('M');
    expect(map.get('src/main.rs')).toBe('M');
    expect(map.size).toBe(2);
  });

  it('parses untracked as ??', () => {
    const map = parsePorcelain('?? notes.md');
    expect(map.get('notes.md')).toBe('??');
  });

  it('parses deleted files', () => {
    const map = parsePorcelain(' D old.rs');
    expect(map.get('old.rs')).toBe('D');
  });

  it('parses added files', () => {
    const map = parsePorcelain('A  new.rs');
    expect(map.get('new.rs')).toBe('A');
  });

  it('takes the new path for renames', () => {
    const map = parsePorcelain('R  old.txt -> new.txt');
    expect(map.get('new.txt')).toBe('R');
    expect(map.has('old.txt')).toBe(false);
  });

  it('returns empty map for empty or header-only input', () => {
    expect(parsePorcelain('').size).toBe(0);
    expect(parsePorcelain('\n').size).toBe(0);
    expect(parsePorcelain('## main').size).toBe(0);
  });
});

describe('FilesPanel', () => {
  it('renders tree with directory and file names', async () => {
    vi.mocked(getFsTree).mockResolvedValue([
      { name: 'src', is_dir: true },
      { name: 'main.rs', is_dir: false },
    ]);
    vi.mocked(getAgentGitStatus).mockResolvedValue({ status: '', stderr: '' });

    renderPanel();

    expect(await screen.findByText('src')).toBeTruthy();
    expect(screen.getByText('main.rs')).toBeTruthy();
    expect(screen.getByLabelText('agent.refresh')).toBeTruthy();
  });

  it('annotates modified files with a git status badge', async () => {
    vi.mocked(getFsTree).mockResolvedValue([
      { name: 'src', is_dir: true },
      { name: 'main.rs', is_dir: false },
    ]);
    vi.mocked(getAgentGitStatus).mockResolvedValue({
      status: '## main\nM  main.rs\n M src/helper.ts\n?? notes.md',
      stderr: '',
    });

    renderPanel();

    // main.rs 已暂存修改 → M 徽章；src 目录子代有变更 → 目录也着色
    expect(await screen.findByText('main.rs')).toBeTruthy();
    const mainRow = screen.getByText('main.rs').closest('div')!;
    expect(within(mainRow).getByText('M')).toBeTruthy();
    const srcRow = screen.getByText('src').closest('div')!;
    expect(within(srcRow).getByText('M')).toBeTruthy();
  });

  it('lazily loads children when a directory is expanded', async () => {
    vi.mocked(getFsTree).mockImplementation(async (_ws, path) =>
      path === 'src'
        ? [{ name: 'lib.rs', is_dir: false }]
        : [{ name: 'src', is_dir: true }, { name: 'main.rs', is_dir: false }]
    );
    vi.mocked(getAgentGitStatus).mockResolvedValue({ status: '', stderr: '' });

    renderPanel();
    expect(await screen.findByText('src')).toBeTruthy();

    fireEvent.click(screen.getByLabelText('expand'));
    expect(await screen.findByText('lib.rs')).toBeTruthy();
    expect(getFsTree).toHaveBeenCalledWith('w1', 'src');
  });

  it('shows preview when a file is clicked', async () => {
    vi.mocked(getFsTree).mockResolvedValue([{ name: 'main.rs', is_dir: false }]);
    vi.mocked(getFsFile).mockResolvedValue({ content: 'fn main() {}', truncated: false });
    vi.mocked(getAgentGitStatus).mockResolvedValue({ status: '', stderr: '' });

    renderPanel();
    const file = await screen.findByText('main.rs');
    fireEvent.click(file);

    // 顶栏：返回按钮 + 路径 + 编辑按钮
    expect(await screen.findByLabelText('agent.edit')).toBeTruthy();
    expect(screen.getByLabelText('agent.backToTree')).toBeTruthy();
    // shiki 高亮预览（vi.mock 返回的 HTML）
    expect(await screen.findByText('highlighted-code')).toBeTruthy();
  });

  it('shows truncated banner when file content is truncated', async () => {
    vi.mocked(getFsTree).mockResolvedValue([{ name: 'big.txt', is_dir: false }]);
    vi.mocked(getFsFile).mockResolvedValue({ content: 'x'.repeat(10), truncated: true });
    vi.mocked(getAgentGitStatus).mockResolvedValue({ status: '', stderr: '' });

    renderPanel();
    fireEvent.click(await screen.findByText('big.txt'));
    expect(await screen.findByText('agent.fileTruncated')).toBeTruthy();
  });

  it('shows client offline message when the fs tree API fails', async () => {
    vi.mocked(getFsTree).mockRejectedValue(new Error('503 offline'));

    renderPanel();
    expect(await screen.findByText('agent.clientOffline')).toBeTruthy();
  });
});
