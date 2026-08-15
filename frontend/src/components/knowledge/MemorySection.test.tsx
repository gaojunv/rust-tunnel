// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { PreferencesProvider } from '@/preferences/PreferencesProvider';
import { readCachedPreferences } from '@/preferences/preferencesStore';
import type { AgentMemorySettings } from '@/types';
import MemorySection from './MemorySection';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('@/api/preferences', () => ({
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

const api = vi.hoisted(() => ({
  getMemorySettings: vi.fn(),
  updateMemorySettings: vi.fn(),
  clearMemory: vi.fn(),
  listMemories: vi.fn(),
  createMemory: vi.fn(),
  updateMemory: vi.fn(),
  deleteMemory: vi.fn(),
  pinMemory: vi.fn(),
  listAgentWorkspaces: vi.fn(),
  clientsApi: { list: vi.fn() },
}));

vi.mock('@/api/client', () => ({
  ...api,
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
}));

// SSE 全局单例替身：不建立真实 EventSource
vi.mock('@/api/memoryStream', () => ({
  memoryStream: { subscribe: vi.fn(() => () => {}) },
}));

const settingsFixture: AgentMemorySettings = {
  enabled: true,
  emb_base_url: 'https://emb.example.com/v1',
  emb_api_key: '',
  emb_model: 'text-embedding-3-small',
  emb_dimension: 1536,
  distill_model: '',
  top_k: 8,
  score_threshold: 0.4,
  inject_budget_tokens: 1500,
  pin_always_inject: true,
  skill_enabled: false,
  skill_list_max: 20,
  has_key: true,
  created_at: '',
  updated_at: '',
};

const renderSection = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <PreferencesProvider>
        <MemorySection />
      </PreferencesProvider>
    </QueryClientProvider>
  );
};

describe('MemorySection', () => {
  beforeEach(() => {
    api.listMemories.mockResolvedValue({ memories: [], total: 0 });
    api.getMemorySettings.mockResolvedValue(settingsFixture);
    api.listAgentWorkspaces.mockResolvedValue([]);
    api.clientsApi.list.mockResolvedValue([]);
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
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('renders memory settings card and empty list state', async () => {
    renderSection();

    expect(screen.getByText('memory.settings.title')).toBeTruthy();
    // 空态：无记忆 + 未选中详情引导
    expect(await screen.findByText('memory.empty')).toBeTruthy();
    expect(screen.getByText('memory.noSelection')).toBeTruthy();
    // 新建记忆按钮存在
    expect(screen.getByText('memory.newMemory')).toBeTruthy();
  });

  it('renders memory cards when the list has data', async () => {
    api.listMemories.mockResolvedValue({
      memories: [
        {
          id: 'm1',
          content: 'user prefers rust over go',
          scope_type: 'global',
          client_id: '',
          workspace_id: '',
          tags: ['rust'],
          confidence: 0.9,
          source_session_id: 's1',
          source_trigger: 'distill',
          pinned: true,
          hit_count: 3,
          last_hit_at: null,
          created_at: '2026-08-01T00:00:00Z',
          updated_at: '2026-08-02T00:00:00Z',
        },
      ],
      total: 1,
    });

    renderSection();

    expect(await screen.findByText('user prefers rust over go')).toBeTruthy();
    expect(screen.queryByText('memory.empty')).toBeNull();
    expect(screen.getByText('memory.listTitle (1)')).toBeTruthy();
  });
});
