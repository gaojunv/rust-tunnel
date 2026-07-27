// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import i18n from '@/i18n';
import { I18nProvider } from '@/i18n/I18nProvider';
import { ThemeProvider } from '@/theme/ThemeProvider';
import { PreferencesProvider } from '@/preferences/PreferencesProvider';
import { PREFERENCES_CACHE_KEY, readCachedPreferences } from '@/preferences/preferencesStore';
import { Header } from './Header';

// Polyfill window.matchMedia for jsdom (used by Logo component)
if (typeof window !== 'undefined' && !window.matchMedia) {
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => {},
    }),
  });
}

vi.mock('../../api/preferences', () => ({
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

// DataFlowBackground uses Three.js WebGL + requestAnimationFrame which hangs
// in jsdom — mock it away to keep tests fast and reliable.
vi.mock('../dataflow/DataFlowBackground.tsx', () => ({
  default: () => null,
}));

const renderHeader = () =>
  render(
    <MemoryRouter>
      <PreferencesProvider>
        <ThemeProvider>
          <I18nProvider>
            <Header onLogout={() => {}} />
          </I18nProvider>
        </ThemeProvider>
      </PreferencesProvider>
    </MemoryRouter>,
  );

describe('Header i18n', () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  beforeEach(() => {
    localStorage.clear();
  });

  it('renders navigation in English', async () => {
    await i18n.changeLanguage('en');
    renderHeader();

    // Dashboard is always visible as a direct nav link
    expect(screen.getAllByText('Dashboard').length).toBeGreaterThan(0);
    // Group labels are visible on DropdownMenu triggers
    expect(screen.getAllByText('Network').length).toBeGreaterThan(0);
    expect(screen.getAllByText('System').length).toBeGreaterThan(0);
  });

  it('renders navigation in Chinese when preference is zh-CN', async () => {
    localStorage.setItem(
      PREFERENCES_CACHE_KEY,
      JSON.stringify({ theme: 'dark', language: 'zh-CN', titleEffect: 'grid-wave' }),
    );
    renderHeader();

    expect(screen.getAllByText('仪表盘').length).toBeGreaterThan(0);
    expect(screen.getAllByText('网络').length).toBeGreaterThan(0);
    expect(screen.getAllByText('系统').length).toBeGreaterThan(0);
  });
});
