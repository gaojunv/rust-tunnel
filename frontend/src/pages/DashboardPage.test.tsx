// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { MemoryRouter } from 'react-router-dom';
import type { AxiosResponse } from 'axios';
import { clientsApi, api } from '@/api/client';
import { PreferencesProvider } from '@/preferences/PreferencesProvider';
import { readCachedPreferences } from '@/preferences/preferencesStore';
import DashboardPage from './DashboardPage';

vi.mock('../api/preferences', () => ({
  fetchPreferences: () => {
    try {
      const cached = readCachedPreferences(
        typeof window !== 'undefined' ? window.localStorage : undefined,
      );
      return Promise.resolve({
        theme: cached.theme,
        language: cached.language,
        title_effect: cached.titleEffect,
      });
    } catch {
      return Promise.resolve({ theme: 'dark', language: 'system', title_effect: 'grid-wave' });
    }
  },
  updatePreferences: () => Promise.resolve(),
}));

const listSpy = vi.spyOn(clientsApi, 'list');
const getSpy = vi.spyOn(api, 'get');

const renderPage = () => {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <MemoryRouter>
      <QueryClientProvider client={queryClient}>
        <PreferencesProvider>
          <DashboardPage />
        </PreferencesProvider>
      </QueryClientProvider>
    </MemoryRouter>
  );
};

const entitySummary = {
  total_bytes_in: 0,
  total_bytes_out: 0,
  total_conns: 0,
  entity_count: 0,
};

const statsSummaryResponse = {
  data: {
    clients: { ...entitySummary, entity_count: 1 },
    proxy: entitySummary,
    shadowsocks: entitySummary,
    trojan: entitySummary,
  },
  status: 200,
  statusText: 'OK',
  headers: {},
  config: {},
  request: {},
} as AxiosResponse;

const emptyResponse = {
  data: {},
  status: 200,
  statusText: 'OK',
  headers: {},
  config: {},
  request: {},
} as AxiosResponse;

describe('DashboardPage', () => {
  beforeEach(() => {
    listSpy.mockReset();
    getSpy.mockReset();
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
    getSpy.mockImplementation((url: string) => {
      if (url === '/stats/summary') {
        return Promise.resolve(statsSummaryResponse);
      }
      if (typeof url === 'string' && url.startsWith('/stats/query')) {
        return Promise.resolve({
          ...emptyResponse,
          data: { snapshots: [] },
        });
      }
      return Promise.resolve(emptyResponse);
    });
    vi.stubGlobal(
      'EventSource',
      class {
        readyState = 1;
        onerror = null;
        addEventListener = vi.fn();
        close = vi.fn();
      }
    );
  });

  it('renders clients without crashing when backend returns wrapped clients', async () => {
    listSpy.mockResolvedValue([
      {
        name: 'home-nas',
        hostname: 'nas.local',
        note: null,
        online: true,
        connected_at: new Date().toISOString(),
        last_seen_at: new Date().toISOString(),
        first_seen_at: new Date().toISOString(),
        client_version: '0.4.0',
        referenced_by_rules: 0,
      },
    ]);

    renderPage();

    await waitFor(() => expect(listSpy).toHaveBeenCalled(), { timeout: 2000 });

    await waitFor(() => {
      expect(screen.queryByText('home-nas')).toBeTruthy();
      expect(screen.queryByText('nas.local')).toBeTruthy();
    });
  });
});
