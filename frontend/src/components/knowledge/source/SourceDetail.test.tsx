// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import SourceDetail from './SourceDetail';
import type { KnowledgeSource } from '@/types';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string, _opts?: unknown) => k }),
}));

const api = vi.hoisted(() => ({
  toggleKnowledgeSource: vi.fn(),
  deleteKnowledgeSource: vi.fn(),
  queryKnowledge: vi.fn(),
  listWikiPages: vi.fn(),
  getWikiPage: vi.fn(),
  searchWiki: vi.fn(),
  listKnowledgeDocs: vi.fn(),
  uploadKnowledgeDoc: vi.fn(),
  deleteKnowledgeDoc: vi.fn(),
  reindexKnowledgeDoc: vi.fn(),
  getWikiGraph: vi.fn(),
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

vi.mock('@/api/client', () => ({
  ...api,
  getApiErrorMessage: api.getApiErrorMessage,
  listKnowledgeSources: vi.fn(),
  listWikiPages: api.listWikiPages,
}));

vi.mock('@/api/hooks', async () => {
  const actual = await vi.importActual('@/api/hooks') as Record<string, unknown>;
  return {
    ...actual,
    useDeleteKnowledgeSource: () => ({ mutate: api.deleteKnowledgeSource, isPending: false }),
    useToggleKnowledgeSource: () => ({ mutate: api.toggleKnowledgeSource, isPending: false }),
    useKnowledgeQuery: () => ({ mutate: api.queryKnowledge, isPending: false, isError: false, data: null }),
    useWikiGraph: () => ({ data: { nodes: [], edges: [] }, isLoading: false }),
    useKnowledgeDocs: () => ({ data: [], isLoading: false }),
    useWikiPages: () => ({ data: { pages: [] }, isLoading: false }),
    useWikiSearch: () => ({ data: { hits: [] }, isFetching: false }),
    useWikiPage: () => ({ data: null, isLoading: false }),
    useKnowledgeStream: () => {},
  };
});

vi.mock('@/api/knowledgeStream', () => ({
  knowledgeStream: { subscribe: vi.fn(() => () => {}) },
}));

const sourceFixture: KnowledgeSource = {
  id: 'k1',
  name: 'kb1',
  summary: 'summary',
  description: 'summary',
  index_vector: true,
  index_pages: true,
  scope_type: 'global',
  client_id: '',
  workspace_id: '',
  emb_base_url: '',
  emb_api_key: '',
  has_api_key: false,
  emb_model: '',
  emb_dimension: 0,
  top_k: 5,
  chunk_size: 512,
  chunk_overlap: 64,
  score_threshold: 0.3,
  status: 'ready',
  version: 1,
  page_count: 0,
  enabled: true,
  doc_count: 0,
  created_at: '',
  updated_at: '',
};

function renderDetail(source: KnowledgeSource = sourceFixture) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <SourceDetail source={source} onBack={vi.fn()} onDeleted={vi.fn()} />
    </QueryClientProvider>,
  );
}

describe('SourceDetail', () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('切换 tab 后检索 query 输入保留（forceMount）', async () => {
    renderDetail();
    const input = screen.getByPlaceholderText('ks.queryPlaceholder') as HTMLInputElement;
    fireEvent.change(input, { target: { value: 'hello' } });
    expect(input.value).toBe('hello');
    // 切换到页面 tab 再切回
    fireEvent.click(screen.getByText('ks.tabPages'));
    fireEvent.click(screen.getByText('ks.tabQuery'));
    expect((screen.getByPlaceholderText('ks.queryPlaceholder') as HTMLInputElement).value).toBe('hello');
  });

  it('toggle 失败显示错误横幅', async () => {
    // 覆盖 toggle mock 为调用 onError
    api.toggleKnowledgeSource.mockImplementation((_args: unknown, opts: { onError?: (e: Error) => void }) => {
      opts?.onError?.(new Error('toggle failed'));
    });
    renderDetail();
    // Radix Switch 触发 onCheckedChange：直接调用需找到 switch role
    const sw = screen.getByLabelText('ks.enabledSwitch');
    fireEvent.click(sw);
    await waitFor(() => expect(screen.getByText(/ks.actionError/)).toBeTruthy());
  });
});
