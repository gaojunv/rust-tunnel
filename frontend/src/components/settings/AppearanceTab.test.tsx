// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import i18n from '@/i18n';
import { I18nProvider } from '@/i18n/I18nProvider';
import { PreferencesProvider } from '@/preferences/PreferencesProvider';
import { PREFERENCES_CACHE_KEY, readCachedPreferences } from '@/preferences/preferencesStore';
import AppearanceTab from './AppearanceTab';

// Mock preferences API to resolve immediately (prevents XHR hang between tests).
// Read the language from the localStorage cache so the mock matches expectations.
vi.mock('../../api/preferences', () => ({
  // Note: the mock factory runs at module load time, so closure captures the
  // module scope. readCachedPreferences is called at fetchPreferences invocation time.
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

const renderTab = () =>
  render(
    <PreferencesProvider>
      <I18nProvider>
        <AppearanceTab />
      </I18nProvider>
    </PreferencesProvider>,
  );

describe('AppearanceTab', () => {
  afterEach(() => {
    cleanup();
  });

  beforeEach(async () => {
    localStorage.clear();
    await i18n.changeLanguage('en');
  });

  it('shows the language section', async () => {
    await i18n.changeLanguage('en');
    renderTab();

    expect(screen.getByText('Appearance')).toBeTruthy();
    expect(screen.getByText('Language')).toBeTruthy();
  });

  it('renders Chinese labels when preference is zh-CN', async () => {
    localStorage.setItem(
      PREFERENCES_CACHE_KEY,
      JSON.stringify({ theme: 'dark', language: 'zh-CN', titleEffect: 'grid-wave' }),
    );
    renderTab();

    // Wait for I18nProvider to apply resolvedLanguage
    const titles = await screen.findAllByText('外观');
    expect(titles.length).toBeGreaterThanOrEqual(1);
    const labels = await screen.findAllByText('语言');
    expect(labels.length).toBeGreaterThanOrEqual(1);
  });
});
