// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import KbDialog from './KbDialog';
import type { LlmKnowledgeBase } from '../../../types';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const captured = vi.hoisted(() => ({
  updateLlmKb: vi.fn(),
  testEmbedding: vi.fn(),
}));

const baseKb: LlmKnowledgeBase = {
  id: 'kb1',
  name: 'My KB',
  description: 'desc',
  emb_base_url: 'https://old.example.com/v1',
  emb_api_key: '',
  emb_model: 'old-model',
  emb_dimension: 768,
  top_k: 5,
  chunk_size: 512,
  chunk_overlap: 64,
  score_threshold: 0.3,
  enabled: true,
  doc_count: 0,
  chunk_count: 0,
  created_at: '2026-08-20T00:00:00Z',
  updated_at: '2026-08-20T00:00:00Z',
};

vi.mock('@/api/hooks', () => ({
  useLlmKbs: () => ({ data: [baseKb] }),
  useMemorySettings: () => ({ data: null }),
  useCreateLlmKb: () => ({ mutate: vi.fn(), isPending: false }),
  useUpdateLlmKb: () => ({ mutate: captured.updateLlmKb, isPending: false }),
  useTestEmbedding: () => ({ mutate: captured.testEmbedding, isPending: false }),
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const renderDialog = (kbId: string | null = null) => {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return render(
    <QueryClientProvider client={qc}>
      <KbDialog open onClose={vi.fn()} kbId={kbId} onCreated={vi.fn()} />
    </QueryClientProvider>,
  );
};

describe('KbDialog edit-mode embedding', () => {
  it('renders prefilled emb fields and keep-key placeholder', async () => {
    renderDialog('kb1');
    // 预填：base url / model / dimension
    expect((await screen.findByDisplayValue('https://old.example.com/v1')) as HTMLInputElement).toBeTruthy();
    expect((await screen.findByDisplayValue('old-model')) as HTMLInputElement).toBeTruthy();
    expect((await screen.findByDisplayValue('768')) as HTMLInputElement).toBeTruthy();
    // api_key 为 password 输入、值为空、placeholder 用 embApiKeyKeep
    const apiKey = (await screen.findByPlaceholderText('kb.embApiKeyKeep')) as HTMLInputElement;
    expect(apiKey.value).toBe('');
    expect(apiKey.getAttribute('type')).toBe('password');
    // 未改动时不显示重建警告
    expect(screen.queryByText('kb.embRebuildWarning')).toBeNull();
  });

  it('changing dimension shows rebuild warning, pops confirm, submits new emb payload', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(true);
    renderDialog('kb1');
    const dim = (await screen.findByDisplayValue('768')) as HTMLInputElement;
    fireEvent.change(dim, { target: { value: '1024' } });
    // 改动后显示 amber 重建警告
    expect(await screen.findByText('kb.embRebuildWarning')).toBeTruthy();
    fireEvent.click(await screen.findByText('common.save'));
    // 弹出确认且确认后提交
    await waitFor(() => expect(confirmSpy).toHaveBeenCalledWith('kb.reindexAllConfirm'));
    await waitFor(() => expect(captured.updateLlmKb).toHaveBeenCalled());
    const [vars] = captured.updateLlmKb.mock.calls[0];
    expect(vars.id).toBe('kb1');
    expect(vars.emb_base_url).toBe('https://old.example.com/v1');
    expect(vars.emb_model).toBe('old-model');
    expect(vars.emb_dimension).toBe(1024);
    expect(vars.emb_api_key).toBe('');
    confirmSpy.mockRestore();
  });

  it('canceling confirm does not submit', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(false);
    renderDialog('kb1');
    const dim = (await screen.findByDisplayValue('768')) as HTMLInputElement;
    fireEvent.change(dim, { target: { value: '1024' } });
    fireEvent.click(await screen.findByText('common.save'));
    await waitFor(() => expect(confirmSpy).toHaveBeenCalled());
    expect(captured.updateLlmKb).not.toHaveBeenCalled();
    confirmSpy.mockRestore();
  });

  it('no emb change skips confirm but still submits emb payload', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(true);
    renderDialog('kb1');
    fireEvent.click(await screen.findByText('common.save'));
    await waitFor(() => expect(captured.updateLlmKb).toHaveBeenCalled());
    expect(confirmSpy).not.toHaveBeenCalled();
    const [vars] = captured.updateLlmKb.mock.calls[0];
    expect(vars.emb_base_url).toBe('https://old.example.com/v1');
    expect(vars.emb_model).toBe('old-model');
    expect(vars.emb_dimension).toBe(768);
    confirmSpy.mockRestore();
  });

  it('test embedding in edit mode sends kb_id', async () => {
    renderDialog('kb1');
    fireEvent.click(await screen.findByText('kb.testEmbedding'));
    await waitFor(() => expect(captured.testEmbedding).toHaveBeenCalled());
    const [req] = captured.testEmbedding.mock.calls[0];
    expect(req.kb_id).toBe('kb1');
    expect(req.base_url).toBe('https://old.example.com/v1');
    expect(req.model).toBe('old-model');
  });
});
