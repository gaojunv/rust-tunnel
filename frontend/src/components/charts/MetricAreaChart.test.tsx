// @vitest-environment jsdom
import { cleanup, render } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { MetricAreaChart } from './MetricAreaChart';

class ResizeObserverMock {
  observe() {}
  unobserve() {}
  disconnect() {}
}

beforeEach(() => {
  vi.stubGlobal('ResizeObserver', ResizeObserverMock);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const series = [{ dataKey: 'rtt', label: 'RTT (ms)', colorVar: 'hsl(var(--chart-1))' }];

describe('MetricAreaChart', () => {
  it('renders empty state when data is empty', () => {
    const { getByText, container } = render(
      <MetricAreaChart
        data={[]}
        series={series}
        yFormatter={(v) => `${v}`}
        emptyText="No RTT data available"
      />,
    );
    expect(getByText('No RTT data available')).toBeTruthy();
    expect(container.querySelector('.recharts-responsive-container')).toBeNull();
  });

  it('renders chart container when data is present', () => {
    const data = [
      { timestamp: '2026-07-19T10:00:00Z', rtt: 12.3 },
      { timestamp: '2026-07-19T10:01:00Z', rtt: 15.1 },
    ];
    const { container, queryByText } = render(
      <MetricAreaChart
        data={data}
        series={series}
        yFormatter={(v) => `${v} ms`}
        emptyText="No RTT data available"
      />,
    );
    expect(container.querySelector('.recharts-responsive-container')).not.toBeNull();
    expect(queryByText('No RTT data available')).toBeNull();
  });
});
