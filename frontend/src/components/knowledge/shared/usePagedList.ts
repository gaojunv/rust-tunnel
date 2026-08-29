import { useCallback, useEffect, useRef, useState } from 'react';

interface UsePagedListOptions<T> {
  fetchPage: (offset: number, limit: number) => Promise<{ items: T[]; total: number }>;
  filtersKey: unknown;
  pageSize?: number;
}

interface UsePagedListReturn<T> {
  items: T[];
  total: number;
  loading: boolean;
  loadingMore: boolean;
  hasMore: boolean;
  loadMore: () => Promise<void>;
  error: unknown;
  reload: () => Promise<void>;
}

/**
 * 分页加载 hook：封装“加载更多”逻辑。
 * - filtersKey 变化时清空已累积 items 并从 offset 0 重新拉取
 * - 并发防护：loading/loadingMore 期间再次触发 loadMore 会被忽略
 * - 组件卸载后不 setState
 */
export function usePagedList<T>({ fetchPage, filtersKey, pageSize = 20 }: UsePagedListOptions<T>): UsePagedListReturn<T> {
  const [items, setItems] = useState<T[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<unknown>(null);

  const mountedRef = useRef(true);
  const fetchPageRef = useRef(fetchPage);
  fetchPageRef.current = fetchPage;
  const loadingRef = useRef(false);
  const loadingMoreRef = useRef(false);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;

    async function loadInitial() {
      setItems([]);
      setTotal(0);
      setError(null);
      setLoading(true);
      setLoadingMore(false);
      loadingRef.current = true;
      try {
        const res = await fetchPageRef.current(0, pageSize);
        if (!mountedRef.current || cancelled) return;
        setItems(res.items);
        setTotal(res.total);
      } catch (e: unknown) {
        if (!mountedRef.current || cancelled) return;
        setError(e);
      } finally {
        if (!cancelled && mountedRef.current) setLoading(false);
        loadingRef.current = false;
      }
    }

    void loadInitial();
    return () => {
      cancelled = true;
    };
  }, [filtersKey, pageSize]);

  const hasMore = items.length < total;

  const loadMore = useCallback(async () => {
    if (loadingRef.current || loadingMoreRef.current) return;
    if (!hasMore) return;
    loadingMoreRef.current = true;
    setLoadingMore(true);
    setError(null);
    try {
      const res = await fetchPageRef.current(items.length, pageSize);
      if (!mountedRef.current) return;
      setItems((prev) => [...prev, ...res.items]);
      setTotal(res.total);
    } catch (e: unknown) {
      if (!mountedRef.current) return;
      setError(e);
    } finally {
      if (mountedRef.current) setLoadingMore(false);
      loadingMoreRef.current = false;
    }
  }, [items.length, pageSize, hasMore]);

  const reload = useCallback(async () => {
    if (loadingRef.current) return;
    setError(null);
    setLoading(true);
    loadingRef.current = true;
    try {
      const res = await fetchPageRef.current(0, pageSize);
      if (!mountedRef.current) return;
      setItems(res.items);
      setTotal(res.total);
    } catch (e: unknown) {
      if (!mountedRef.current) return;
      setError(e);
    } finally {
      if (mountedRef.current) setLoading(false);
      loadingRef.current = false;
    }
  }, [pageSize]);

  return { items, total, loading, loadingMore, hasMore, loadMore, error, reload };
}
