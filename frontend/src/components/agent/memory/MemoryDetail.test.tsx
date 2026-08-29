// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi, beforeEach } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { AgentMemory } from '@/types';
import MemoryDetail from './MemoryDetail';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string, _o?: unknown) => k }),
}));

const toastSpy = vi.hoisted(() => ({ success: vi.fn(), error: vi.fn() }));
vi.mock('sonner', () => ({ toast: toastSpy }));

const api = vi.hoisted(() => ({
  updateMemory: vi.fn(),
  deleteMemory: vi.fn(),
  pinMemory: vi.fn(),
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

vi.mock('@/api/client', () => api);

// 用可控的 mutation mock
const updateMock = vi.fn();
const deleteMock = vi.fn();
const pinMock = vi.fn();

vi.mock('@/api/hooks', () => ({
  useUpdateMemory: () => ({ mutate: updateMock, isPending: false }),
  useDeleteMemory: () => ({ mutate: deleteMock, isPending: false }),
  usePinMemory: () => ({ mutate: pinMock, isPending: false }),
}));

vi.mock('@/utils/format', () => ({
  formatDateTime: (s: string) => `fmt:${s}`,
  formatBytes: (n: number) => `${n} B`,
  formatBps: (n: number) => `${n} B/s`,
  formatMs: (n: number) => `${n} ms`,
  formatPercent: (n: number) => `${n}%`,
}));

const memoryFixture: AgentMemory = {
  id: 'm1',
  content: 'user prefers rust over go and this is a very long content that should be clamped to two lines in the detail header',
  scope_type: 'global',
  client_id: '',
  workspace_id: '',
  tags: ['rust'],
  confidence: 0.9,
  source_session_id: 's1',
  source_trigger: 'distill',
  pinned: false,
  hit_count: 3,
  last_hit_at: '2026-08-02T10:00:00Z',
  created_at: '2026-08-01T00:00:00Z',
  updated_at: '2026-08-02T00:00:00Z',
};

const renderDetail = (mem: AgentMemory = memoryFixture) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryDetail memory={mem} onBack={vi.fn()} onDeleted={vi.fn()} />
    </QueryClientProvider>,
  );
};

describe('MemoryDetail', () => {
  beforeEach(() => {
    updateMock.mockImplementation((_args: unknown, _opts?: { onSuccess?: () => void; onError?: (e: unknown) => void }) => {
      void _opts;
      // 默认不回调，由测试按需 mockImplementation
    });
    deleteMock.mockImplementation((_id: string, _opts?: { onSuccess?: () => void; onError?: (e: unknown) => void }) => {
      _opts?.onSuccess?.();
    });
    pinMock.mockImplementation((_id: string, _opts?: { onSuccess?: () => void; onError?: (e: unknown) => void }) => { void _opts; });
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('长标题 line-clamp-2 且带 title，时间字段走 formatDateTime', () => {
    renderDetail();
    const titleEl = screen.getByTitle(memoryFixture.content);
    expect(titleEl.className).toContain('line-clamp-2');
    expect(screen.getByText('fmt:2026-08-02T10:00:00Z')).toBeTruthy();
    expect(screen.getByText('fmt:2026-08-01T00:00:00Z')).toBeTruthy();
    expect(screen.getByText('fmt:2026-08-02T00:00:00Z')).toBeTruthy();
  });

  it('删除走 ConfirmDialog 且不调用 window.confirm', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm');
    const onDeleted = vi.fn();
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <MemoryDetail memory={memoryFixture} onBack={vi.fn()} onDeleted={onDeleted} />
      </QueryClientProvider>,
    );
    // 点击删除应弹出 ConfirmDialog 而非 window.confirm
    fireEvent.click(screen.getByText('common.delete'));
    // ConfirmDialog 标题来自拆分后的 key
    expect(await screen.findByText('memory.deleteConfirmTitle')).toBeTruthy();
    expect(confirmSpy).not.toHaveBeenCalled();
    // 确认删除
    fireEvent.click(screen.getByText('common.confirm'));
    await waitFor(() => expect(deleteMock).toHaveBeenCalled());
    confirmSpy.mockRestore();
  });

  it('删除成功触发 toast.success', async () => {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    deleteMock.mockImplementation((_id: string, _opts?: { onSuccess?: () => void }) => _opts?.onSuccess?.());
    render(
      <QueryClientProvider client={qc}>
        <MemoryDetail memory={memoryFixture} onBack={vi.fn()} onDeleted={vi.fn()} />
      </QueryClientProvider>,
    );
    fireEvent.click(screen.getByText('common.delete'));
    fireEvent.click(await screen.findByText('common.confirm'));
    await waitFor(() => expect(toastSpy.success).toHaveBeenCalledWith('common.toast.deleted'));
  });

  it('pin 失败显示错误横幅', async () => {
    pinMock.mockImplementation((_id: string, _opts?: { onError?: (e: unknown) => void }) => {
     _opts?.onError?.(new Error('pin boom'));
    });
    renderDetail();
    fireEvent.click(screen.getByLabelText('memory.pinnedSwitch'));
    expect(await screen.findByText('memory.saveError')).toBeTruthy();
  });

  it('内联表单为唯一编辑路径，不渲染头部编辑按钮', () => {
    renderDetail();
    // 头部不应再有编辑按钮
    expect(screen.queryByText('common.edit')).toBeNull();
    // 内联表单存在
    expect(screen.getByLabelText('memory.content')).toBeTruthy();
  });

  it('last_hit_at 为空时显示 never', () => {
    renderDetail({ ...memoryFixture, last_hit_at: null });
    expect(screen.getByText('memory.never')).toBeTruthy();
  });
});
