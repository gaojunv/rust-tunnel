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

    // embedding 字段已移至页面顶部的共享设置（SharedEmbeddingSettings），此处不应渲染
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

  it('clears all memories via the clear button', async () => {
    api.getMemorySettings.mockResolvedValue(settingsFixture);
    api.clearMemory.mockResolvedValue({});
    renderSettings();

    await screen.findByDisplayValue('deepseek-chat');
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(true);
    fireEvent.click(screen.getByText('memory.settings.clear'));

    await waitFor(() => {
      expect(api.clearMemory).toHaveBeenCalled();
    });
    confirmSpy.mockRestore();
  });
});
