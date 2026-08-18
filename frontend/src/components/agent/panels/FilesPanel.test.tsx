// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import FilesPanel, { parsePorcelain } from './FilesPanel';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('../../../api/client', () => ({
  getFsTree: vi.fn(),
  getFsFile: vi.fn(),
  putFsFile: vi.fn(),
  getAgentGitStatus: vi.fn(),
  getApiErrorMessage: vi.fn((err: unknown) => String(err)),
}));

import { getAgentGitStatus, getFsFile, getFsTree, putFsFile } from '../../../api/client';

beforeEach(() => {
  localStorage.clear();
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const renderPanel = (workspaceId = 'w1') => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return {
    qc,
    ...render(
      <QueryClientProvider client={qc}>
        <FilesPanel workspaceId={workspaceId} />
      </QueryClientProvider>
    ),
  };
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

  it('opens a file tab on tree click and previews content', async () => {
    vi.mocked(getFsTree).mockResolvedValue([{ name: 'main.rs', is_dir: false }]);
    vi.mocked(getFsFile).mockResolvedValue({ content: 'fn main() {}', truncated: false });
    vi.mocked(getAgentGitStatus).mockResolvedValue({ status: '', stderr: '' });

    renderPanel();
    fireEvent.click(await screen.findByText('main.rs'));

    // 顶栏：返回按钮 + 路径 + 编辑按钮；标签条出现该文件
    expect(await screen.findByLabelText('agent.edit')).toBeTruthy();
    expect(screen.getByLabelText('agent.backToTree')).toBeTruthy();
    const tablist = await screen.findByRole('tablist');
    expect(within(tablist).getByText('main.rs')).toBeTruthy();
    // 只读预览：jsdom 下 CodeMirrorEditor 退化为纯文本 pre（替代原 shiki 高亮）
    expect(await screen.findByText('fn main() {}')).toBeTruthy();
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

  it('renders multiple restored tabs and switches the active one', async () => {
    localStorage.setItem(
      'agent.files.w1',
      JSON.stringify({ open: ['a.rs', 'b.rs'], active: 'a.rs' }),
    );
    vi.mocked(getFsTree).mockResolvedValue([
      { name: 'a.rs', is_dir: false },
      { name: 'b.rs', is_dir: false },
    ]);
    vi.mocked(getFsFile).mockImplementation(async (_ws, path) =>
      path === 'a.rs'
        ? { content: 'AAA', truncated: false }
        : { content: 'BBB', truncated: false }
    );
    vi.mocked(getAgentGitStatus).mockResolvedValue({ status: '', stderr: '' });

    renderPanel();

    const tablist = await screen.findByRole('tablist');
    expect(within(tablist).getByText('a.rs')).toBeTruthy();
    expect(within(tablist).getByText('b.rs')).toBeTruthy();

    // 初始 active = a.rs：内容可见，tab 高亮
    const viewA = await screen.findByTestId('file-tab-view-a.rs');
    expect(await within(viewA).findByText('AAA')).toBeTruthy();
    expect(
      within(tablist).getByText('a.rs').closest('[role="tab"]')!.getAttribute('aria-selected'),
    ).toBe('true');

    // 切换到 b.rs：active 翻转且内容出现
    fireEvent.click(within(tablist).getByText('b.rs'));
    expect(
      within(tablist).getByText('b.rs').closest('[role="tab"]')!.getAttribute('aria-selected'),
    ).toBe('true');
    expect(await within(screen.getByTestId('file-tab-view-b.rs')).findByText('BBB')).toBeTruthy();
  });

  it('closes one tab and activates the right neighbor', async () => {
    localStorage.setItem(
      'agent.files.w1',
      JSON.stringify({ open: ['a.rs', 'b.rs'], active: 'a.rs' }),
    );
    vi.mocked(getFsTree).mockResolvedValue([
      { name: 'a.rs', is_dir: false },
      { name: 'b.rs', is_dir: false },
    ]);
    vi.mocked(getFsFile).mockImplementation(async (_ws, path) => ({
      content: `${path} content`,
      truncated: false,
    }));
    vi.mocked(getAgentGitStatus).mockResolvedValue({ status: '', stderr: '' });

    renderPanel();

    const tablist = await screen.findByRole('tablist');
    expect(within(tablist).getByText('b.rs')).toBeTruthy();

    fireEvent.click(within(tablist).getAllByLabelText('agent.closeFile')[0]);

    expect(within(tablist).queryByText('a.rs')).toBeNull();
    expect(within(tablist).getByText('b.rs')).toBeTruthy();
    expect(
      within(tablist).getByText('b.rs').closest('[role="tab"]')!.getAttribute('aria-selected'),
    ).toBe('true');
    // 状态已持久化
    expect(JSON.parse(localStorage.getItem('agent.files.w1')!)).toEqual({
      open: ['b.rs'],
      active: 'b.rs',
    });
  });

  it('edits a file, shows the unsaved dot, and saves via putFsFile', async () => {
    vi.mocked(getFsTree).mockResolvedValue([{ name: 'main.rs', is_dir: false }]);
    vi.mocked(getFsFile).mockResolvedValue({ content: 'fn main() {}', truncated: false });
    vi.mocked(getAgentGitStatus).mockResolvedValue({ status: '', stderr: '' });

    renderPanel();
    fireEvent.click(await screen.findByText('main.rs'));
    // 等文件内容就绪（Edit 按钮在查询完成前是禁用态）
    expect(await screen.findByText('fn main() {}')).toBeTruthy();
    fireEvent.click(screen.getByLabelText('agent.edit'));

    // jsdom 下编辑框退化为 textarea
    const textarea = await screen.findByTestId('file-editor');
    fireEvent.change(textarea, { target: { value: 'fn main() {}\n// edited' } });

    // 未保存圆点出现在标签 title 上
    expect(await screen.findByTitle('agent.unsavedChanges · main.rs')).toBeTruthy();

    fireEvent.click(screen.getByLabelText('agent.save'));
    await waitFor(() => {
      expect(putFsFile).toHaveBeenCalledWith('w1', 'main.rs', 'fn main() {}\n// edited', false);
    });
    // 保存成功后退出编辑态（返回预览），未保存标记清除
    expect(await screen.findByLabelText('agent.edit')).toBeTruthy();
    expect(screen.queryByTitle('agent.unsavedChanges · main.rs')).toBeNull();
  });

  it('resets open tabs when the workspace changes', async () => {
    localStorage.setItem('agent.files.w1', JSON.stringify({ open: ['a.rs'], active: 'a.rs' }));
    localStorage.setItem('agent.files.w2', JSON.stringify({ open: ['b.rs'], active: 'b.rs' }));
    vi.mocked(getFsTree).mockResolvedValue([
      { name: 'a.rs', is_dir: false },
      { name: 'b.rs', is_dir: false },
    ]);
    vi.mocked(getFsFile).mockImplementation(async (_ws, path) => ({
      content: `${path} content`,
      truncated: false,
    }));
    vi.mocked(getAgentGitStatus).mockResolvedValue({ status: '', stderr: '' });

    const { qc, rerender } = renderPanel('w1');
    expect(await screen.findByRole('tablist')).toBeTruthy();

    rerender(
      <QueryClientProvider client={qc}>
        <FilesPanel workspaceId="w2" />
      </QueryClientProvider>
    );

    // w2 的标签恢复，旧 workspace 的标签被清空
    const tablist = await screen.findByRole('tablist');
    expect(within(tablist).getByText('b.rs')).toBeTruthy();
    expect(within(tablist).queryByText('a.rs')).toBeNull();
  });
});
