// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import WikiPagesTab from './WikiPagesTab';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const api = vi.hoisted(() => ({
  listWikiPages: vi.fn(),
  getWikiPage: vi.fn(),
  searchWiki: vi.fn(),
  deleteWikiPage: vi.fn(),
  putWikiPage: vi.fn(),
}));

vi.mock('@/api/client', () => ({
  ...api,
  getApiErrorMessage: (e: unknown) => (e as Error).message ?? String(e),
  listKnowledgeSources: vi.fn(),
  listWikiPages: api.listWikiPages,
  getWikiPage: api.getWikiPage,
  searchWiki: api.searchWiki,
}));

describe('WikiPagesTab C', () => {
  beforeEach(() => {
    api.listWikiPages.mockResolvedValue({ pages: [{ id: 'p1', wiki_id: 'w1', ref: 'a/b', title: 'T', summary: '', locked: false, use_count: 1, created_at: '', updated_at: '' }], total: 1 });
    api.searchWiki.mockResolvedValue({ hits: [] });
    api.getWikiPage.mockResolvedValue({ id: 'p1', wiki_id: 'w1', ref: 'a/b', title: 'T', summary: '', content: 'hello', locked: false, use_count: 1, created_at: '', updated_at: '' });
  });
  afterEach(() => { cleanup(); vi.clearAllMocks(); });

  it('编辑按钮 aria 为 common.edit', async () => {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <WikiPagesTab wikiId="w1" defaultOpenRef="a/b" />
      </QueryClientProvider>,
    );
    expect(await screen.findByLabelText('common.edit')).toBeTruthy();
  });

  it('搜索框有内容时显示清除按钮', async () => {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <WikiPagesTab wikiId="w1" />
      </QueryClientProvider>,
    );
    const input = screen.getByLabelText('wiki.pageSearchPlaceholder') as HTMLInputElement;
    fireEvent.change(input, { target: { value: 'hello' } });
    expect(await screen.findByLabelText('common.clearSearch')).toBeTruthy();
    fireEvent.click(screen.getByLabelText('common.clearSearch'));
    expect(input.value).toBe('');
  });
});
