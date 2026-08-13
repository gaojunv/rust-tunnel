// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import GitPanel, { parsePorcelainEntries } from './GitPanel';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('../../../api/client', () => ({
  getAgentGitStatus: vi.fn(),
  getAgentGitDiff: vi.fn(),
  listAgentMessages: vi.fn().mockResolvedValue([]),
}));

import { getAgentGitDiff, getAgentGitStatus, listAgentMessages } from '../../../api/client';

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const renderPanel = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <GitPanel sessionId="s1" workspaceId="w1" />
    </QueryClientProvider>
  );
};

describe('parsePorcelainEntries', () => {
  it('skips branch header and parses staged/unstaged modified', () => {
    const entries = parsePorcelainEntries(`## main...origin/main
 M src/main.rs
M  src/lib.rs
`);
    expect(entries).toHaveLength(2);
    expect(entries[0]).toMatchObject({
      path: 'src/main.rs',
      x: ' ',
      y: 'M',
      status: 'modified',
      staged: false,
    });
    expect(entries[1]).toMatchObject({
      path: 'src/lib.rs',
      x: 'M',
      y: ' ',
      status: 'modified',
      staged: true,
    });
  });

  it('parses untracked files', () => {
    const entries = parsePorcelainEntries('?? new-file.txt');
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({
      path: 'new-file.txt',
      status: 'untracked',
      staged: false,
    });
  });

  it('parses deleted files', () => {
    const entries = parsePorcelainEntries(' D old.rs');
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({
      path: 'old.rs',
      status: 'deleted',
      staged: false,
    });
  });

  it('parses renamed lines and takes the new path', () => {
    const entries = parsePorcelainEntries('R  old.txt -> new.txt');
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({
      path: 'new.txt',
      status: 'renamed',
      staged: true,
    });
  });

  it('returns empty for empty or header-only input', () => {
    expect(parsePorcelainEntries('')).toEqual([]);
    expect(parsePorcelainEntries('\n')).toEqual([]);
    expect(parsePorcelainEntries('## main')).toEqual([]);
  });
});

describe('GitPanel', () => {
  it('renders grouped entries and expands a diff on click', async () => {
    vi.mocked(getAgentGitStatus).mockResolvedValue({
      status: `## main
M  src/lib.rs
 M src/main.rs
?? notes.md
`,
      stderr: '',
    });
    vi.mocked(getAgentGitDiff).mockResolvedValue(`diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,3 @@
-let x = 1;
+let x = 2;
`);

    renderPanel();

    expect(await screen.findByText('agent.stagedChanges')).toBeTruthy();
    expect(screen.getByText('agent.changes')).toBeTruthy();
    expect(screen.getByText('agent.untracked')).toBeTruthy();

    expect(screen.getByText('src/lib.rs')).toBeTruthy();
    expect(screen.getByText('src/main.rs')).toBeTruthy();
    expect(screen.getByText('notes.md')).toBeTruthy();

    // 展开 src/main.rs 的 diff
    fireEvent.click(screen.getByRole('button', { name: /src\/main\.rs/ }));
    expect(await screen.findByText('+let x = 2;')).toBeTruthy();
    expect(screen.getByText('-let x = 1;')).toBeTruthy();

    // 再次点击折叠
    fireEvent.click(screen.getByRole('button', { name: /src\/main\.rs/ }));
    expect(screen.queryByText('+let x = 2;')).toBeNull();
  });

  it('shows not-git-repo state when stderr is non-empty', async () => {
    vi.mocked(getAgentGitStatus).mockResolvedValue({
      status: '',
      stderr: 'fatal: not a git repository (or any of the parent directories): .git',
    });

    renderPanel();
    expect(await screen.findByText('agent.notGitRepo')).toBeTruthy();
  });

  it('falls back to cached git_status tool result when status API errors', async () => {
    vi.mocked(getAgentGitStatus).mockRejectedValue(new Error('503 offline'));
    vi.mocked(listAgentMessages).mockResolvedValue({
      messages: [
        {
          id: 'm1',
          session_id: 's1',
          role: 'tool',
          content: ' M src/main.rs\n?? notes.md',
          name: 'git_status',
          kind: 'tool_result',
          created_at: '',
        },
        {
          id: 'm2',
          session_id: 's1',
          role: 'assistant',
          content: 'some unrelated text',
          name: null,
          kind: 'text',
          created_at: '',
        },
      ],
      has_more: false,
    });

    renderPanel();
    expect(await screen.findByText(/src\/main\.rs/)).toBeTruthy();
    expect(screen.getByText(/notes\.md/)).toBeTruthy();
  });
});
