// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import WikiPageDialog from './WikiPageDialog';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('@/api/client', () => ({
  getApiErrorMessage: (e: unknown) => (e as Error).message ?? String(e),
}));

const api = vi.hoisted(() => ({
  putWikiPage: vi.fn(),
}));
vi.mock('@/api/hooks', () => ({
  usePutWikiPage: () => ({ mutate: api.putWikiPage, isPending: false }),
}));

function renderDialog(content = 'hello **world**') {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <WikiPageDialog wikiId="w1" open onClose={vi.fn()} page={{ id: 'p1', wiki_id: 'w1', ref: 'a/b', title: 'T', summary: '', content, locked: false, use_count: 0, created_at: '', updated_at: '' }} />
    </QueryClientProvider>,
  );
}

describe('WikiPageDialog preview', () => {
  afterEach(() => { cleanup(); vi.clearAllMocks(); });
  it('编辑/预览切换', () => {
    renderDialog('hello **world**');
    expect(screen.getByText('wiki.editTab')).toBeTruthy();
    expect(screen.getByText('wiki.previewTab')).toBeTruthy();
    expect(document.querySelector('textarea')).toBeTruthy();
    fireEvent.click(screen.getByText('wiki.previewTab'));
    expect(document.querySelector('textarea')).toBeNull();
    fireEvent.click(screen.getByText('wiki.editTab'));
    expect(document.querySelector('textarea')).toBeTruthy();
  });
});
