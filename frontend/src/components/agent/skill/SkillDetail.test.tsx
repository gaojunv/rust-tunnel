// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { AgentSkill } from '@/types';
import SkillDetail from './SkillDetail';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string, _o?: unknown) => k }),
}));

const toastSpy = vi.hoisted(() => ({ success: vi.fn(), error: vi.fn() }));
vi.mock('sonner', () => ({ toast: toastSpy }));

vi.mock('@/api/client', () => ({
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

const toggleMock = vi.fn();
const deleteMock = vi.fn();

vi.mock('@/api/hooks', () => ({
  useSkill: () => ({ data: null }),
  useToggleSkill: () => ({ mutate: toggleMock, isPending: false }),
  useDeleteSkill: () => ({ mutate: deleteMock, isPending: false }),
}));

vi.mock('@/components/agent/Markdown', () => ({
  default: ({ content }: { content: string }) => <div>{content}</div>,
}));

vi.mock('./SkillDialog', () => ({
  default: () => null,
}));

vi.mock('@/utils/format', () => ({
  formatDateTime: (s: string) => `fmt:${s}`,
  formatBytes: (n: number) => `${n} B`,
  formatBps: (n: number) => `${n} B/s`,
  formatMs: (n: number) => `${n} ms`,
  formatPercent: (n: number) => `${n}%`,
}));

const skillFixture: AgentSkill = {
  id: 's1',
  name: 'Release checklist',
  description: 'Run before every release',
  content: '1. run tests',
  scope_type: 'global',
  client_id: '',
  workspace_id: '',
  tags: ['deploy'],
  enabled: true,
  source_session_id: 's1',
  source_trigger: 'distill',
  use_count: 3,
  last_used_at: '2026-08-02T11:00:00Z',
  created_at: '2026-08-01T00:00:00Z',
  updated_at: '2026-08-02T00:00:00Z',
};

const renderDetail = (skill: AgentSkill = skillFixture) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <SkillDetail skill={skill} onBack={vi.fn()} onDeleted={vi.fn()} />
    </QueryClientProvider>,
  );
};

describe('SkillDetail', () => {
  beforeEach(() => {
    toggleMock.mockImplementation((_id: string, _opts?: { onError?: (e: unknown) => void }) => { void _opts; });
    deleteMock.mockImplementation((_id: string, _opts?: { onSuccess?: () => void }) => _opts?.onSuccess?.());
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('时间字段走 formatDateTime', () => {
    renderDetail();
    expect(screen.getByText('fmt:2026-08-02T11:00:00Z')).toBeTruthy();
    expect(screen.getByText('fmt:2026-08-01T00:00:00Z')).toBeTruthy();
  });

  it('last_used_at 为空时显示 never', () => {
    renderDetail({ ...skillFixture, last_used_at: null });
    expect(screen.getByText('skill.never')).toBeTruthy();
  });

  it('删除走 ConfirmDialog 且不调用 window.confirm', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm');
    renderDetail();
    fireEvent.click(screen.getByText('common.delete'));
    expect(await screen.findByText('skill.deleteConfirmTitle')).toBeTruthy();
    expect(confirmSpy).not.toHaveBeenCalled();
    fireEvent.click(screen.getByText('common.confirm'));
    await waitFor(() => expect(deleteMock).toHaveBeenCalled());
    confirmSpy.mockRestore();
  });

  it('删除成功触发 toast.success', async () => {
    deleteMock.mockImplementation((_id: string, _opts?: { onSuccess?: () => void }) => _opts?.onSuccess?.());
    renderDetail();
    fireEvent.click(screen.getByText('common.delete'));
    fireEvent.click(await screen.findByText('common.confirm'));
    await waitFor(() => expect(toastSpy.success).toHaveBeenCalledWith('common.toast.deleted'));
  });

  it('启停失败显示错误横幅', async () => {
    toggleMock.mockImplementation((_id: string, _opts?: { onError?: (e: unknown) => void }) => {
     _opts?.onError?.(new Error('toggle boom'));
    });
    renderDetail();
    fireEvent.click(screen.getByLabelText('skill.enabledSwitch'));
    expect(await screen.findByText('skill.saveError')).toBeTruthy();
  });
});
