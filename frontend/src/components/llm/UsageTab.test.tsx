// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import UsageTab from './UsageTab';
import type { LlmUsageSummary } from '../../types';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string, p?: Record<string, unknown>) =>
    p && 'count' in p ? `${k}:${p['count']}/${p['total']}` : k }),
}));

vi.mock('@/hooks/useTimeRange', () => ({
  useTimeRange: () => ({
    range: { preset: '24h', startMs: 0, endMs: 1000 },
    preset: '24h',
    presets: ['24h'],
    setPreset: vi.fn(),
    setCustomRange: vi.fn(),
  }),
}));

const baseSummary: LlmUsageSummary = {
  requests: 10,
  success: 9,
  prompt_tokens: 1000,
  cache_hit_tokens: 200,
  cache_miss_tokens: 800,
  completion_tokens: 500,
  total_tokens: 1500,
  failover_count: 2,
};

const mockState = vi.hoisted(() => ({
  summaryData: undefined as LlmUsageSummary | undefined,
  summaryError: false,
}));

vi.mock('@/api/hooks', async (orig) => {
  const actual = await orig<typeof import('@/api/hooks')>();
  return {
    ...actual,
    useLlmUsageSummary: vi.fn(() => ({
      data: mockState.summaryData,
      isError: mockState.summaryError,
    })),
    useLlmUsageAggregate: vi.fn(() => ({ data: [], isLoading: false, isError: false })),
    useLlmUsageLogs: vi.fn(() => ({
      data: { logs: [], total: 0 },
      isLoading: false,
      isError: false,
      isFetching: false,
    })),
  };
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  mockState.summaryData = undefined;
  mockState.summaryError = false;
});

describe('UsageTab', () => {
  it('转移率读 summary 汇总口径（failover_count/requests），非分页样本', () => {
    mockState.summaryData = baseSummary;
    render(<UsageTab />);

    // failoverRateDesc 带 count/total 插值：2/10
    expect(screen.getByText('llm.usage.failoverRateDesc:2/10')).toBeTruthy();
    // 卡片值为 pct(2,10) = 20.0%（缓存命中率同样 20.0%，出现两次，断言数组）
    expect(screen.getAllByText('20.0%').length).toBeGreaterThanOrEqual(2);
  });

  it('summary 加载失败时渲染错误横幅且卡片显示占位', () => {
    mockState.summaryError = true;
    render(<UsageTab />);

    expect(screen.getByText('llm.usage.loadFailed')).toBeTruthy();
    expect(screen.getAllByText('—').length).toBeGreaterThan(0);
  });
});
