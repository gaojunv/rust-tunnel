// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { KnowledgeSource } from '@/types';
import SourceSection from './SourceSection';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const api = vi.hoisted(() => ({
  listKnowledgeSources: vi.fn(),
  createKnowledgeSource: vi.fn(),
  updateKnowledgeSource: vi.fn(),
  deleteKnowledgeSource: vi.fn(),
  toggleKnowledgeSource: vi.fn(),
  listKnowledgeDocs: vi.fn(),
  listAgentWorkspaces: vi.fn(),
  getMemorySettings: vi.fn(),
  clientsApi: { list: vi.fn() },
}));

vi.mock('@/api/client', () => ({
  ...api,
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

// SSE 全局单例替身：不建立真实 EventSource
vi.mock('@/api/knowledgeStream', () => ({
  knowledgeStream: { subscribe: vi.fn(() => () => {}) },
}));

const sourceFixture: KnowledgeSource = {
  id: 'k1',
  name: 'deploy-kb',
  summary: 'Deployment runbooks',
  description: 'Deployment runbooks',
  index_vector: true,
  index_pages: true,
  scope_type: 'workspace',
  client_id: '',
  workspace_id: 'ws1',
  emb_base_url: 'https://api.openai.com/v1',
  emb_api_key: '',
  has_api_key: true,
  emb_model: 'text-embedding-3-small',
  emb_dimension: 1536,
  top_k: 5,
  chunk_size: 512,
  chunk_overlap: 64,
  score_threshold: 0.3,
  status: 'ready',
  version: 2,
  page_count: 5,
  enabled: true,
  doc_count: 3,
  created_at: '2026-08-01T00:00:00Z',
  updated_at: '2026-08-02T00:00:00Z',
};

const renderSection = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <SourceSection />
    </QueryClientProvider>,
  );
};

describe('SourceSection', () => {
  beforeEach(() => {
    api.listKnowledgeSources.mockResolvedValue({ sources: [sourceFixture], total: 1 });
    api.listKnowledgeDocs.mockResolvedValue([]);
    api.listAgentWorkspaces.mockResolvedValue([{ id: 'ws1', name: 'default' }]);
    api.clientsApi.list.mockResolvedValue([]);
    // 全局 embedding 未配置：创建时走自定义分支，不出现"使用全局配置"开关
    api.getMemorySettings.mockResolvedValue({
      emb_base_url: '',
      emb_api_key: '',
      emb_model: '',
      emb_dimension: 0,
    });
    api.createKnowledgeSource.mockResolvedValue({ ...sourceFixture, id: 'k2', name: 'ops-kb' });
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('renders container cards with both index badges', async () => {
    renderSection();
    expect(await screen.findByText('deploy-kb')).toBeTruthy();
    expect(screen.getByText('Deployment runbooks')).toBeTruthy();
    // 双索引都开 → 两个侧徽章同时出现
    expect(screen.getByText('ks.badgeVector')).toBeTruthy();
    expect(screen.getByText('ks.badgePages')).toBeTruthy();
    // status/scope 文案与筛选下拉 option 同 key，用 getAllByText
    expect(screen.getAllByText('ks.status.ready').length).toBeGreaterThan(0);
    expect(screen.getAllByText('ks.scope_workspace').length).toBeGreaterThan(0);
    expect(screen.queryByText('ks.empty')).toBeNull();
  });

  it('shows the empty state when there are no containers', async () => {
    api.listKnowledgeSources.mockResolvedValue({ sources: [], total: 0 });
    renderSection();
    expect(await screen.findByText('ks.empty')).toBeTruthy();
    expect(screen.getByText('ks.noSelection')).toBeTruthy();
  });

  it('creates a pages-only container without embedding fields', async () => {
    renderSection();
    fireEvent.click(await screen.findByText('ks.new'));

    fireEvent.change(screen.getByLabelText('ks.name'), { target: { value: 'ops-kb' } });
    fireEvent.change(screen.getByLabelText('ks.summary'), { target: { value: 'Ops runbooks' } });
    fireEvent.change(screen.getByLabelText('ks.workspaceLabel'), { target: { value: 'ws1' } });

    // 关向量、开页面：pages-only 容器不需要 embedding，保存不应被 embRequired 挡住
    fireEvent.click(screen.getByLabelText('ks.indexVector'));
    fireEvent.click(screen.getByLabelText('ks.indexPages'));

    fireEvent.click(screen.getByText('common.save'));

    await waitFor(() => {
      expect(api.createKnowledgeSource).toHaveBeenCalledWith({
        name: 'ops-kb',
        summary: 'Ops runbooks',
        index_vector: false,
        index_pages: true,
        scope_type: 'workspace',
        workspace_id: 'ws1',
      });
    });
  });

  it('blocks creation when neither index side is enabled', async () => {
    renderSection();
    fireEvent.click(await screen.findByText('ks.new'));

    fireEvent.change(screen.getByLabelText('ks.name'), { target: { value: 'ops-kb' } });
    fireEvent.click(screen.getByLabelText('ks.indexVector'));

    expect(screen.getByText('ks.indexRequired')).toBeTruthy();
    fireEvent.click(screen.getByText('common.save'));
    expect(api.createKnowledgeSource).not.toHaveBeenCalled();
  });
});
