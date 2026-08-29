// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { AgentMemorySettings } from '../../../types';
import MemorySettings from './MemorySettings';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const api = vi.hoisted(() => ({
  getMemorySettings: vi.fn(),
  updateMemorySettings: vi.fn(),
  clearMemory: vi.fn(),
}));

vi.mock('../../../api/client', () => ({
  ...api,
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

vi.mock('@/utils/format', () => ({
  formatDateTime: (s: string) => `fmt:${s}`,
  formatBytes: (n: number) => `${n} B`,
  formatBps: (n: number) => `${n} B/s`,
  formatMs: (n: number) => `${n} ms`,
  formatPercent: (n: number) => `${n}%`,
}));

const settingsFixture: AgentMemorySettings = {
  enabled: true,
  emb_base_url: 'https://emb.example.com/v1',
  emb_api_key: '',
  emb_model: 'text-embedding-3-small',
  emb_dimension: 1536,
  distill_model: 'deepseek-chat',
  top_k: 8,
  score_threshold: 0.4,
  inject_budget_tokens: 1500,
  pin_always_inject: true,
  skill_enabled: false,
  skill_list_max: 20,
  wiki_enabled: true,
  wiki_list_max: 20,
  has_key: true,
  created_at: '',
  updated_at: '',
};

const renderSettings = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemorySettings />
    </QueryClientProvider>
  );
};

describe('MemorySettings', () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('loads memory-specific settings and saves without embedding fields', async () => {
    api.getMemorySettings.mockResolvedValue(settingsFixture);
    api.updateMemorySettings.mockResolvedValue(settingsFixture);
    renderSettings();

    expect(screen.queryByLabelText('memory.settings.embBaseUrl')).toBeNull();

    await screen.findByDisplayValue('deepseek-chat');
    fireEvent.change(screen.getByLabelText('memory.settings.distillModel'), {
      target: { value: 'gpt-4o-mini' },
    });
    fireEvent.click(screen.getByText('memory.settings.save'));

    await waitFor(() => {
      expect(api.updateMemorySettings).toHaveBeenCalledWith({
        enabled: true,
        distill_model: 'gpt-4o-mini',
        top_k: 8,
        score_threshold: 0.4,
        inject_budget_tokens: 1500,
        pin_always_inject: true,
      });
    });
  });

  it('组件内不再渲染 h3 标题（外层 DialogHeader 已有）', async () => {
    api.getMemorySettings.mockResolvedValue(settingsFixture);
    renderSettings();
    await screen.findByDisplayValue('deepseek-chat');
    // MemorySettings 内不再渲染 <h3>memory.settings.title</h3>
    const headings = screen.queryAllByRole('heading', { level: 3 });
    expect(headings.length).toBe(0);
    // 描述文字仍保留
    expect(screen.getByText('memory.settings.enabledDesc')).toBeTruthy();
  });

  it('清空走 ConfirmDialog 而非 window.confirm', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm');
    api.getMemorySettings.mockResolvedValue(settingsFixture);
    api.clearMemory.mockResolvedValue({});
    renderSettings();

    await screen.findByDisplayValue('deepseek-chat');
    fireEvent.click(screen.getByText('memory.settings.clear'));

    // 应弹出 ConfirmDialog（标题为拆分后的 clearConfirmTitle）
    expect(await screen.findByText('memory.settings.clearConfirmTitle')).toBeTruthy();
    expect(confirmSpy).not.toHaveBeenCalled();

    fireEvent.click(screen.getByText('common.confirm'));
    await waitFor(() => {
      expect(api.clearMemory).toHaveBeenCalled();
    });
    confirmSpy.mockRestore();
  });
});
