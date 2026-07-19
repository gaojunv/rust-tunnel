// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { MemoryRouter } from 'react-router-dom';
import type { AxiosResponse } from 'axios';
import { clientsApi, api } from '@/api/client';
import DashboardPage from './DashboardPage';

const listSpy = vi.spyOn(clientsApi, 'list');
const getSpy = vi.spyOn(api, 'get');

const renderPage = () => {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <MemoryRouter>
      <QueryClientProvider client={queryClient}>
        <DashboardPage />
      </QueryClientProvider>
    </MemoryRouter>
  );
};

const metricsResponse = {
  data: {
    client_count: 1,
    active_connection_count: 0,
    total_bytes_in: 0,
    total_bytes_out: 0,
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
    getSpy.mockImplementation((url: string) => {
      if (url === '/metrics') {
        return Promise.resolve(metricsResponse);
      }
      return Promise.resolve(emptyResponse);
    });
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
