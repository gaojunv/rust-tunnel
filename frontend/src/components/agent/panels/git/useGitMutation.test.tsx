// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import type { ReactNode } from 'react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { useGitMutation } from './useGitMutation';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

// 与真实实现同构的最小 getApiErrorMessage mock（string / {error} / Error 兜底）
vi.mock('../../../../api/client', () => ({
  getApiErrorMessage: (err: unknown): string => {
    const data = (err as { response?: { data?: unknown } })?.response?.data;
    if (typeof data === 'string') return data;
    if (data && typeof data === 'object') {
      const msg = (data as { error?: unknown }).error;
      if (typeof msg === 'string') return msg;
    }
    return err instanceof Error ? err.message : String(err);
  },
}));

const makeClient = () =>
  new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });

const wrapper = (qc: QueryClient) =>
  function Wrapper({ children }: { children: ReactNode }) {
    return <QueryClientProvider client={qc}>{children}</QueryClientProvider>;
  };

/** 模拟 409 审批响应体。 */
const approval409 = (summary: string) => ({
  response: { status: 409, data: { needs_approval: true, summary } },
});

describe('useGitMutation', () => {
  it('prompts approval on 409 needs_approval and re-sends with approved=true', async () => {
    const fn = vi
      .fn()
      .mockRejectedValueOnce(approval409('git push'))
      .mockResolvedValueOnce(undefined);
    const qc = makeClient();
    const { result } = renderHook(() => useGitMutation(fn), { wrapper: wrapper(qc) });

    result.current.mutate('origin');

    await waitFor(() => expect(result.current.approval).not.toBeNull());
    expect(result.current.approval?.summary).toBe('git push');
    expect(fn).toHaveBeenLastCalledWith(false, 'origin');

    result.current.confirmApproval();

    await waitFor(() => expect(result.current.approval).toBeNull());
    await waitFor(() => expect(fn).toHaveBeenCalledTimes(2));
    expect(fn).toHaveBeenLastCalledWith(true, 'origin');
  });

  it('cancelApproval drops the pending prompt without re-sending', async () => {
    const fn = vi.fn().mockRejectedValueOnce(approval409('git revert abc'));
    const qc = makeClient();
    const { result } = renderHook(() => useGitMutation(fn), { wrapper: wrapper(qc) });

    result.current.mutate('abc');
    await waitFor(() => expect(result.current.approval).not.toBeNull());

    result.current.cancelApproval();
    await waitFor(() => expect(result.current.approval).toBeNull());
    expect(fn).toHaveBeenCalledTimes(1);
  });

  it('surfaces needs_upgrade message on old-client 409', async () => {
    const fn = vi
      .fn()
      .mockRejectedValueOnce({
        response: { status: 409, data: { needs_upgrade: true, message: 'client too old' } },
      });
    const qc = makeClient();
    const { result } = renderHook(() => useGitMutation(fn), { wrapper: wrapper(qc) });

    result.current.mutate();
    await waitFor(() => expect(result.current.error).toBe('client too old'));
    expect(result.current.approval).toBeNull();
  });

  it('falls back to agent.gitUpgradeRequired when needs_upgrade has no message', async () => {
    const fn = vi
      .fn()
      .mockRejectedValueOnce({ response: { status: 409, data: { needs_upgrade: true } } });
    const qc = makeClient();
    const { result } = renderHook(() => useGitMutation(fn), { wrapper: wrapper(qc) });

    result.current.mutate();
    await waitFor(() => expect(result.current.error).toBe('agent.gitUpgradeRequired'));
  });

  it('propagates plain API errors via getApiErrorMessage', async () => {
    const fn = vi
      .fn()
      .mockRejectedValueOnce({ response: { status: 503, data: { error: 'boom' } } });
    const qc = makeClient();
    const { result } = renderHook(() => useGitMutation(fn), { wrapper: wrapper(qc) });

    result.current.mutate();
    await waitFor(() => expect(result.current.error).toBe('boom'));
  });

  it('runs onSuccess and clears error after a successful mutation', async () => {
    const fn = vi.fn().mockResolvedValueOnce(undefined);
    const onSuccess = vi.fn();
    const qc = makeClient();
    const { result } = renderHook(() => useGitMutation(fn, { onSuccess }), {
      wrapper: wrapper(qc),
    });

    result.current.mutate('x');
    await waitFor(() => expect(onSuccess).toHaveBeenCalledTimes(1));
    expect(result.current.error).toBeNull();
    expect(result.current.approval).toBeNull();
  });
});
