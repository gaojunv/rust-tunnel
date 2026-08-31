// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';
import { renderHook, act, waitFor } from '@testing-library/react';
import { usePagedList } from './usePagedList';

function makeFetch(total: number, delay = 0) {
  return vi.fn(async (offset: number, limit: number) => {
    if (delay) await new Promise((r) => setTimeout(r, delay));
    const remaining = Math.max(0, total - offset);
    const count = Math.min(limit, remaining);
    const items = Array.from({ length: count }, (_, i) => `item-${offset + i}`);
    return { items, total };
  });
}

describe('usePagedList', () => {
  it('初始加载 items/total/loading 状态正确', async () => {
    const fetchPage = makeFetch(5);
    const { result } = renderHook(() => usePagedList({ fetchPage, filtersKey: 'a', pageSize: 2 }));
    expect(result.current.loading).toBe(true);
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.items).toEqual(['item-0', 'item-1']);
    expect(result.current.total).toBe(5);
    expect(result.current.hasMore).toBe(true);
  });

  it('loadMore 累加 items', async () => {
    const fetchPage = makeFetch(5);
    const { result } = renderHook(() => usePagedList({ fetchPage, filtersKey: 'a', pageSize: 2 }));
    await waitFor(() => expect(result.current.loading).toBe(false));
    await act(async () => {
      await result.current.loadMore();
    });
    expect(result.current.items).toEqual(['item-0', 'item-1', 'item-2', 'item-3']);
    await act(async () => {
      await result.current.loadMore();
    });
    expect(result.current.items).toEqual(['item-0', 'item-1', 'item-2', 'item-3', 'item-4']);
    expect(result.current.hasMore).toBe(false);
  });

  it('filtersKey 变化重置 items 并重新拉取', async () => {
    const fetchPage = makeFetch(10);
    let key: unknown = 'a';
    const { result, rerender } = renderHook(() => usePagedList({ fetchPage, filtersKey: key, pageSize: 2 }));
    await waitFor(() => expect(result.current.loading).toBe(false));
    await act(async () => {
      await result.current.loadMore();
    });
    expect(result.current.items.length).toBe(4);
    key = 'b';
    rerender();
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.items).toEqual(['item-0', 'item-1']);
  });

  it('并发 loadMore 第二次被忽略', async () => {
    const fetchPage = makeFetch(10, 20);
    const { result } = renderHook(() => usePagedList({ fetchPage, filtersKey: 'a', pageSize: 2 }));
    await waitFor(() => expect(result.current.loading).toBe(false));
    fetchPage.mockClear();
    const p1 = result.current.loadMore();
    const p2 = result.current.loadMore();
    await act(async () => {
      await Promise.all([p1, p2]);
    });
    expect(fetchPage).toHaveBeenCalledTimes(1);
  });

  it('total 正确透出', async () => {
    const fetchPage = makeFetch(42);
    const { result } = renderHook(() => usePagedList({ fetchPage, filtersKey: 'k', pageSize: 10 }));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.total).toBe(42);
  });
});
