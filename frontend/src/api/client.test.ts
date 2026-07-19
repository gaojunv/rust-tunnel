// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';
import type { AxiosResponse } from 'axios';
import { api, clientsApi } from './client';

describe('clientsApi.list', () => {
  it('returns an array extracted from the wrapped clients response', async () => {
    const response = {
      data: {
        clients: [
          {
            name: 'test-client',
            hostname: 'test.local',
            note: null,
            online: true,
            connected_at: null,
            last_seen_at: new Date().toISOString(),
            first_seen_at: new Date().toISOString(),
            client_version: null,
            referenced_by_rules: 0,
          },
        ],
      },
      status: 200,
      statusText: 'OK',
      headers: {},
      config: {},
      request: {},
    } as AxiosResponse;

    const getSpy = vi.spyOn(api, 'get').mockResolvedValueOnce(response);

    const clients = await clientsApi.list();

    expect(Array.isArray(clients)).toBe(true);
    expect(clients).toHaveLength(1);
    expect(clients[0].name).toBe('test-client');
    expect(api.get).toHaveBeenCalledWith('/clients');

    getSpy.mockRestore();
  });
});
