// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen, fireEvent } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import MentionPopup, { filterMentionCandidates } from './MentionPopup';
import type { AgentRole } from '../../types';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('../../api/client', () => ({
  listWorkspaceFiles: vi.fn().mockResolvedValue({ files: ['src/main.ts', 'src/app.tsx'] }),
}));

const roleFixture: AgentRole = {
  id: 'r1',
  name: 'code-reviewer',
  description: 'Reviews code',
  system_prompt: '',
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

const disabledRole: AgentRole = {
  ...roleFixture,
  id: 'r2',
  name: 'disabled-role',
  enabled: false,
};

describe('filterMentionCandidates', () => {
  it('returns roles matching query before files', () => {
    const result = filterMentionCandidates('code', ['src/main.ts'], [roleFixture]);
    expect(result).toHaveLength(1);
    expect(result[0].kind).toBe('role');
    expect(result[0].value).toBe('code-reviewer');
  });

  it('filters out disabled roles', () => {
    const result = filterMentionCandidates('disabled', [], [disabledRole]);
    expect(result).toHaveLength(0);
  });

  it('returns files when no roles match', () => {
    const result = filterMentionCandidates('main', ['src/main.ts'], [roleFixture]);
    expect(result).toHaveLength(1);
    expect(result[0].kind).toBe('file');
    expect(result[0].value).toBe('src/main.ts');
  });

  it('combines role and file results', () => {
    const result = filterMentionCandidates('src', ['src/main.ts', 'src/app.tsx'], [roleFixture]);
    expect(result).toHaveLength(2);
    expect(result.every((r) => r.kind === 'file')).toBe(true);
  });

  it('returns empty for unmatched query', () => {
    const result = filterMentionCandidates('zzz', ['src/main.ts'], [roleFixture]);
    expect(result).toHaveLength(0);
  });
});

function renderPopup(roles: AgentRole[] = [roleFixture]) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const onSelect = vi.fn();
  const onFilesChange = vi.fn();
  const onActiveIdxChange = vi.fn();
  render(
    <QueryClientProvider client={qc}>
      <MentionPopup
        workspaceId="ws1"
        query=""
        activeIdx={0}
        onActiveIdxChange={onActiveIdxChange}
        onFilesChange={onFilesChange}
        onSelect={onSelect}
        roles={roles}
      />
    </QueryClientProvider>,
  );
  return { onSelect, onFilesChange, onActiveIdxChange };
}

describe('MentionPopup', () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('renders role candidate with Bot icon and role badge', async () => {
    renderPopup();
    // Role should appear in the list
    expect(await screen.findByText('code-reviewer')).toBeTruthy();
    expect(screen.getByText('role.candidateRole')).toBeTruthy();
  });

  it('onSelect is called with @role prefix for role candidates', async () => {
    const { onSelect } = renderPopup();
    fireEvent.click(await screen.findByText('code-reviewer'));
    expect(onSelect).toHaveBeenCalledWith('@code-reviewer');
  });

  it('shows empty message when no candidates', async () => {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <MentionPopup
          workspaceId="ws1"
          query="nonexistent"
          activeIdx={0}
          onActiveIdxChange={vi.fn()}
          onFilesChange={vi.fn()}
          onSelect={vi.fn()}
          roles={[]}
        />
      </QueryClientProvider>,
    );
    expect(await screen.findByText('agent.noMatchingFiles')).toBeTruthy();
  });
});
