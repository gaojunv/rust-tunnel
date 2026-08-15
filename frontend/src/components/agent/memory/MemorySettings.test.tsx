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
  testMemoryEmbedding: vi.fn(),
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
  distill_model: '',
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

  it('loads settings into the form and saves edited values', async () => {
    api.getMemorySettings.mockResolvedValue(settingsFixture);
    api.updateMemorySettings.mockResolvedValue(settingsFixture);
    renderSettings();

    // 等表单从 settings 初始化
    await screen.findByDisplayValue('https://emb.example.com/v1');
    expect((screen.getByLabelText('memory.settings.embDimension') as HTMLInputElement).value).toBe(
      '1536',
    );

    fireEvent.change(screen.getByLabelText('memory.settings.distillModel'), {
      target: { value: 'deepseek-chat' },
    });
    fireEvent.click(screen.getByText('memory.settings.save'));

    await waitFor(() => {
      expect(api.updateMemorySettings).toHaveBeenCalledWith({
        enabled: true,
        emb_base_url: 'https://emb.example.com/v1',
        emb_model: 'text-embedding-3-small',
        emb_dimension: 1536,
        distill_model: 'deepseek-chat',
        top_k: 8,
        score_threshold: 0.4,
        inject_budget_tokens: 1500,
        pin_always_inject: true,
      });
    });
  });

  it('probe dimension calls testEmbedding and auto-fills the dimension input', async () => {
    api.getMemorySettings.mockResolvedValue(settingsFixture);
    api.testMemoryEmbedding.mockResolvedValue({ dimension: 1024, latency_ms: 42 });
    renderSettings();

    await screen.findByDisplayValue('https://emb.example.com/v1');

    fireEvent.change(screen.getByLabelText('memory.settings.embBaseUrl'), {
      target: { value: 'https://new.example.com/v1' },
    });
    fireEvent.change(screen.getByLabelText('memory.settings.embApiKey'), {
      target: { value: 'sk-test' },
    });
    fireEvent.change(screen.getByLabelText('memory.settings.embModel'), {
      target: { value: 'my-emb' },
    });
    fireEvent.click(screen.getByText('memory.settings.testEmbedding'));

    await waitFor(() => {
      expect(api.testMemoryEmbedding).toHaveBeenCalledWith({
        base_url: 'https://new.example.com/v1',
        api_key: 'sk-test',
        model: 'my-emb',
      });
    });
    await waitFor(() => {
      expect(
        (screen.getByLabelText('memory.settings.embDimension') as HTMLInputElement).value,
      ).toBe('1024');
    });
  });
});
