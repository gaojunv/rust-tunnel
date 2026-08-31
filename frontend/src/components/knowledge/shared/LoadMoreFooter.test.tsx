// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import LoadMoreFooter from './LoadMoreFooter';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({
    t: (k: string, opts?: Record<string, unknown>) => (opts ? `${k} ${JSON.stringify(opts)}` : k),
  }),
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe('LoadMoreFooter', () => {
  it('渲染计数文案', () => {
    render(<LoadMoreFooter loaded={5} total={20} onLoadMore={vi.fn()} />);
    expect(screen.getByText(/common.loadedOf/)).toBeTruthy();
    expect(screen.getByText(/"loaded":5/)).toBeTruthy();
  });

  it('hasMore 时按钮可点并触发 onLoadMore', () => {
    const onLoadMore = vi.fn();
    render(<LoadMoreFooter loaded={5} total={20} onLoadMore={onLoadMore} />);
    const btn = screen.getByRole('button', { name: /common.loadMore/ });
    expect(btn).toBeTruthy();
    fireEvent.click(btn);
    expect(onLoadMore).toHaveBeenCalledTimes(1);
  });

  it('loaded>=total 时不显示按钮', () => {
    render(<LoadMoreFooter loaded={20} total={20} onLoadMore={vi.fn()} />);
    expect(screen.queryByRole('button')).toBeNull();
  });

  it('loading 时按钮 disabled', () => {
    render(<LoadMoreFooter loaded={5} total={20} loading onLoadMore={vi.fn()} />);
    const btn = screen.getByRole('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });
});
