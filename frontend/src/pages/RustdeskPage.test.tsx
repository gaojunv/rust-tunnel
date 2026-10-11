// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import Renderer from './RustdeskPage';
import { PreferencesProvider } from '@/preferences/PreferencesProvider';
import type { RustdeskConfig } from '@/types';

// PreferencesProvider 会拉取 /preferences；按 DashboardPage.test 的既有做法 mock 掉。
vi.mock('@/api/preferences', () => ({
  fetchPreferences: () =>
    Promise.resolve({ theme: 'dark', language: 'system', title_effect: 'grid-wave' }),
  updatePreferences: () => Promise.resolve(),
}));

const mocks = vi.hoisted(() => ({
  config: vi.fn(),
  summary: vi.fn(),
  query: vi.fn(),
}));

vi.mock('@/api/hooks', () => ({
  useRustdeskConfig: () => mocks.config(),
  useUpdateRustdeskConfig: () => ({ mutate: vi.fn(), isPending: false }),
  useStatsSummary: () => mocks.summary(),
  useStatsQuery: () => mocks.query(),
}));

const baseConfig: RustdeskConfig = {
  enabled: true,
  rendezvous_port: 21116,
  relay_port: 21117,
  relay_server: 'relay.example.com:21117',
  always_use_relay: false,
  local_ip: '192.168.1.10',
  mask: '192.168.0.0/16',
  key_set: true,
  running: true,
  nat_test_port: 21115,
  online_peers: 3,
  known_peers: 7,
  relay_pairs: 1,
  relay_total_pairs: 12,
  rejected_total: 4,
};

beforeEach(() => {
  // jsdom 无 matchMedia，PageHeader 的 useGridWaveCanvas 会直接调用它（同 DashboardPage.test）
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    value: vi.fn().mockImplementation((query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      addListener: vi.fn(),
      removeListener: vi.fn(),
      dispatchEvent: vi.fn(),
    })),
  });
  mocks.config.mockReturnValue({ data: baseConfig, isLoading: false, isError: false, refetch: vi.fn() });
  mocks.summary.mockReturnValue({ isError: false, refetch: vi.fn() });
  mocks.query.mockReturnValue({ data: [] });
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe('RustdeskPage', () => {
  it('渲染 5 张统计卡，含 known_peers', () => {
    render(
      <PreferencesProvider>
        <Renderer />
      </PreferencesProvider>,
    );

    expect(screen.getByText('Service Status')).toBeTruthy();
    expect(screen.getByText('Active')).toBeTruthy();
    expect(screen.getByText('Online Peers')).toBeTruthy();
    expect(screen.getByText('Known Peers')).toBeTruthy();
    expect(screen.getByText('Relay Pairs')).toBeTruthy();
    // “Throughput” 同时出现在统计卡标题与图表卡片标题上
    expect(screen.getAllByText('Throughput').length).toBeGreaterThan(0);

    // Known Peers 的取值来自 config.known_peers
    expect(screen.getByText('7')).toBeTruthy();
  });

  it('配置卡片展示累计中继对与累计拒绝数（与当前中继对区分）', () => {
    render(
      <PreferencesProvider>
        <Renderer />
      </PreferencesProvider>,
    );

    expect(screen.getByText('1 relay pairs')).toBeTruthy(); // 当前中继对
    expect(screen.getByText('12 relay pairs since start')).toBeTruthy(); // 累计
    expect(screen.getByText('4 rejected since start')).toBeTruthy(); // 累计
  });

  it('无快照时渲染外层 EmptyState', () => {
    render(
      <PreferencesProvider>
        <Renderer />
      </PreferencesProvider>,
    );

    expect(screen.getByText('No throughput data available')).toBeTruthy();
  });
});
