// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { cleanup, render, screen, waitFor, fireEvent } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { MemoryRouter } from 'react-router-dom';
import type { AxiosResponse } from 'axios';
import { api, clientDownloadUrl, wikiDownloadUrl } from '@/api/client';
import { PreferencesProvider } from '@/preferences/PreferencesProvider';
import { readCachedPreferences } from '@/preferences/preferencesStore';
import type { ClientDownloadsResponse } from '@/types';
import DownloadsPage from './DownloadsPage';

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

const getSpy = vi.spyOn(api, 'get');

const wrap = (data: ClientDownloadsResponse) =>
  ({
    data,
    status: 200,
    statusText: 'OK',
    headers: {},
    config: {},
    request: {},
  }) as AxiosResponse;

// 默认停在客户端 tab；未切换时 wiki 内容处于隐藏状态，不干扰客户端断言。
const wrapDownloads =
  (client: ClientDownloadsResponse, wiki?: ClientDownloadsResponse) =>
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  (url: string): any => {
    if (typeof url === 'string' && url.includes('wiki-downloads')) {
      return Promise.resolve(
        wrap(
          wiki ?? {
            dir_available: true,
            configured_dir: '/opt/rust-tunnel/wiki',
            latest: null,
            versions: [],
          },
        ),
      );
    }
    return Promise.resolve(wrap(client));
  };

const renderPage = () => {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <MemoryRouter>
      <QueryClientProvider client={queryClient}>
        <PreferencesProvider>
          <DownloadsPage />
        </PreferencesProvider>
      </QueryClientProvider>
    </MemoryRouter>
  );
};

const populated: ClientDownloadsResponse = {
  dir_available: true,
  configured_dir: '/opt/rust-tunnel/client',
  latest: 'v0.9.0',
  versions: [
    {
      version: 'v0.9.0',
      is_latest: true,
      modified_at: 1_756_000_000,
      files: [
        {
          name: 'rust-tunnel-client-linux-x86_64',
          os: 'linux',
          arch: 'x86_64',
          size: 8_388_608,
          sha256: 'a'.repeat(64),
          format: null,
        },
        {
          name: 'rust-tunnel-client-macos-aarch64',
          os: 'macos',
          arch: 'aarch64',
          size: 7_340_032,
          sha256: null,
          format: null,
        },
      ],
    },
    {
      version: 'v0.8.1',
      is_latest: false,
      modified_at: null,
      files: [
        {
          name: 'rust-tunnel-client-linux-x86_64',
          os: 'linux',
          arch: 'x86_64',
          size: 8_000_000,
          sha256: null,
          format: null,
        },
      ],
    },
  ],
};

const wikiPopulated: ClientDownloadsResponse = {
  dir_available: true,
  configured_dir: '/opt/rust-tunnel/wiki',
  latest: 'v0.9.0',
  versions: [
    {
      version: 'v0.9.0',
      is_latest: true,
      modified_at: 1_756_000_000,
      files: [
        {
          name: 'wiki-desktop-macos-aarch64.dmg',
          os: 'macos',
          arch: 'aarch64',
          size: 12_000_000,
          sha256: 'b'.repeat(64),
          format: 'dmg',
        },
        {
          name: 'wiki-desktop-windows-x86_64.msi',
          os: 'windows',
          arch: 'x86_64',
          size: 15_000_000,
          sha256: null,
          format: 'msi',
        },
        {
          name: 'wiki-desktop-windows-x86_64-setup.exe',
          os: 'windows',
          arch: 'x86_64',
          size: 16_000_000,
          sha256: null,
          format: 'exe',
        },
      ],
    },
  ],
};

describe('DownloadsPage', () => {
  beforeEach(() => {
    getSpy.mockReset();
    localStorage.clear();
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

  // vitest 未开 globals，testing-library 的自动 cleanup 不会注册 —— 手动清理，
  // 否则同文件内前一个用例的 DOM 会残留，让 queryBy* 断言假失败。
  afterEach(cleanup);

  it('renders the latest version with a token-bearing download link', async () => {
    localStorage.setItem('auth_token', 'tok en/1');
    getSpy.mockImplementation(wrapDownloads(populated));
    renderPage();

    await waitFor(() => {
      expect(screen.getByText('Linux · x86_64')).toBeTruthy();
    });
    // latest 徽标只挂在最新版本卡上（wiki tab 隐藏，未计入）
    expect(screen.getAllByText('Latest')).toHaveLength(1);
    expect(screen.getByText('macOS · aarch64')).toBeTruthy();
    // v0.9.0 出现两次：StatCard 摘要 + 版本卡标题（仅客户端 tab 可见）
    expect(screen.getAllByText('v0.9.0')).toHaveLength(2);

    const linux = screen
      .getAllByRole('link')
      .find((a) => a.getAttribute('href')?.includes('rust-tunnel-client-linux-x86_64'));
    expect(linux?.getAttribute('href')).toBe(
      '/api/client-downloads/v0.9.0/rust-tunnel-client-linux-x86_64?token=tok%20en%2F1'
    );
    expect(linux?.getAttribute('download')).toBe('rust-tunnel-client-linux-x86_64');

    // 历史版本默认折叠：v0.8.1 的卡片不在初始 DOM 里
    expect(screen.queryByText('v0.8.1')).toBeNull();
  });

  it('shows a SHA256 copy button only for files that carry a checksum', async () => {
    getSpy.mockImplementation(wrapDownloads(populated));
    renderPage();

    await waitFor(() => {
      expect(screen.getByText('Linux · x86_64')).toBeTruthy();
    });
    // linux 有 sha256、macos 为 null（仅客户端 tab 可见，故为 1）
    expect(screen.getAllByText('SHA256')).toHaveLength(1);
  });

  it('renders an empty state with the configured dir when the archive is unavailable', async () => {
    getSpy.mockImplementation(
      wrapDownloads({
        dir_available: false,
        configured_dir: '/opt/rust-tunnel/client',
        latest: null,
        versions: [],
      }),
    );
    renderPage();

    await waitFor(() => {
      expect(screen.getByText('/opt/rust-tunnel/client')).toBeTruthy();
    });
    expect(screen.queryAllByRole('link')).toHaveLength(0);
  });

  it('renders an empty state when the archive holds no versions', async () => {
    getSpy.mockImplementation(
      wrapDownloads({
        dir_available: true,
        configured_dir: '/opt/rust-tunnel/client',
        latest: null,
        versions: [],
      }),
    );
    renderPage();

    await waitFor(() => {
      expect(screen.getByText(/no client releases published yet/i)).toBeTruthy();
    });
    expect(screen.queryAllByRole('link')).toHaveLength(0);
  });

  // Radix Tabs 在 jsdom 中需 mouseDown+click 触发切换（与 WorkspaceDialog.test 同模式）
  const switchToWikiTab = async () => {
    const tab = screen.getByRole('tab', { name: /wiki/i });
    fireEvent.mouseDown(tab);
    fireEvent.click(tab);
    await waitFor(() => {
      expect(tab.getAttribute('data-state')).toBe('active');
    });
  };

  it('switching to the wiki tab renders wiki artifacts and wiki download links', async () => {
    localStorage.setItem('auth_token', 'tok en/1');
    getSpy.mockImplementation(wrapDownloads(populated, wikiPopulated));
    renderPage();

    // 默认在客户端 tab，等待其加载完成
    await waitFor(() => {
      expect(screen.getByText('Linux · x86_64')).toBeTruthy();
    });

    await switchToWikiTab();

    await waitFor(() => {
      expect(screen.getByText('wiki-desktop-macos-aarch64.dmg')).toBeTruthy();
    });
    expect(screen.getByText('wiki-desktop-windows-x86_64.msi')).toBeTruthy();

    const dmg = screen
      .getAllByRole('link')
      .find((a) => a.getAttribute('href')?.includes('wiki-desktop-macos-aarch64.dmg'));
    expect(dmg?.getAttribute('href')).toBe(
      '/api/wiki-downloads/v0.9.0/wiki-desktop-macos-aarch64.dmg?token=tok%20en%2F1'
    );
    expect(dmg?.getAttribute('download')).toBe('wiki-desktop-macos-aarch64.dmg');
  });

  it('renders format badges for wiki artifacts', async () => {
    getSpy.mockImplementation(wrapDownloads(populated, wikiPopulated));
    renderPage();

    await waitFor(() => {
      expect(screen.getByText('Linux · x86_64')).toBeTruthy();
    });

    await switchToWikiTab();

    await waitFor(() => {
      expect(screen.getByText('DMG')).toBeTruthy();
    });
    expect(screen.getByText('MSI')).toBeTruthy();
    expect(screen.getByText('EXE')).toBeTruthy();
  });
});

describe('clientDownloadUrl', () => {
  beforeEach(() => localStorage.clear());

  it('percent-encodes both path segments and the token', () => {
    localStorage.setItem('auth_token', 'a+b/c');
    expect(clientDownloadUrl('v1.0.0', 'file name.exe')).toBe(
      '/api/client-downloads/v1.0.0/file%20name.exe?token=a%2Bb%2Fc'
    );
  });

  it('omits the query string when no token is stored', () => {
    expect(clientDownloadUrl('v1.0.0', 'bin')).toBe('/api/client-downloads/v1.0.0/bin');
  });
});

describe('wikiDownloadUrl', () => {
  beforeEach(() => localStorage.clear());

  it('percent-encodes both path segments and the token', () => {
    localStorage.setItem('auth_token', 'a+b/c');
    expect(wikiDownloadUrl('v1.0.0', 'file name.dmg')).toBe(
      '/api/wiki-downloads/v1.0.0/file%20name.dmg?token=a%2Bb%2Fc'
    );
  });

  it('omits the query string when no token is stored', () => {
    expect(wikiDownloadUrl('v1.0.0', 'bin.dmg')).toBe('/api/wiki-downloads/v1.0.0/bin.dmg');
  });
});
