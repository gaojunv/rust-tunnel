// @vitest-environment jsdom
import { act, cleanup, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import i18n from './index';
import { I18nProvider, useLanguagePreference } from './I18nProvider';
import { PreferencesProvider } from '../preferences/PreferencesProvider';
import { PREFERENCES_CACHE_KEY } from '../preferences/preferencesStore';

vi.mock('../api/preferences', () => ({
  fetchPreferences: vi.fn().mockResolvedValue({
    theme: 'dark',
    language: 'system',
    title_effect: 'grid-wave',
  }),
  updatePreferences: vi.fn().mockResolvedValue(undefined),
}));

function Probe() {
  const { preference, resolvedLanguage, setPreference } = useLanguagePreference();
  return (
    <div>
      <span data-testid="preference">{preference}</span>
      <span data-testid="resolved">{resolvedLanguage}</span>
      <span data-testid="i18n-lng">{i18n.language}</span>
      <button onClick={() => setPreference('zh-CN')}>to-zh</button>
      <button onClick={() => setPreference('system')}>to-system</button>
    </div>
  );
}

const setNavigatorLanguage = (lang: string) => {
  Object.defineProperty(window.navigator, 'language', {
    value: lang,
    configurable: true,
  });
};

describe('I18nProvider', () => {
  beforeEach(() => {
    localStorage.clear();
    setNavigatorLanguage('en-US');
  });

  afterEach(() => {
    cleanup();
  });

  it('defaults to system preference and resolves from navigator.language', () => {
    setNavigatorLanguage('zh-CN');
    render(
      <PreferencesProvider>
        <I18nProvider>
          <Probe />
        </I18nProvider>
      </PreferencesProvider>,
    );

    expect(screen.getByTestId('preference').textContent).toBe('system');
    expect(screen.getByTestId('resolved').textContent).toBe('zh-CN');
    expect(i18n.language).toBe('zh-CN');
  });

  it('reads stored preference from cache', () => {
    localStorage.setItem(
      PREFERENCES_CACHE_KEY,
      JSON.stringify({ theme: 'dark', language: 'en', titleEffect: 'grid-wave' }),
    );
    setNavigatorLanguage('zh-CN');
    render(
      <PreferencesProvider>
        <I18nProvider>
          <Probe />
        </I18nProvider>
      </PreferencesProvider>,
    );

    expect(screen.getByTestId('resolved').textContent).toBe('en');
  });

  it('setPreference persists and switches i18n language', () => {
    render(
      <PreferencesProvider>
        <I18nProvider>
          <Probe />
        </I18nProvider>
      </PreferencesProvider>,
    );

    act(() => {
      screen.getByText('to-zh').click();
    });

    const cached = JSON.parse(localStorage.getItem(PREFERENCES_CACHE_KEY) || '{}');
    expect(cached.language).toBe('zh-CN');
    expect(screen.getByTestId('resolved').textContent).toBe('zh-CN');
    expect(i18n.language).toBe('zh-CN');
  });

  it('falls back to system for invalid stored value', () => {
    localStorage.setItem(
      PREFERENCES_CACHE_KEY,
      JSON.stringify({ theme: 'dark', language: 'fr', titleEffect: 'grid-wave' }),
    );
    render(
      <PreferencesProvider>
        <I18nProvider>
          <Probe />
        </I18nProvider>
      </PreferencesProvider>,
    );

    expect(screen.getByTestId('preference').textContent).toBe('system');
  });

  it('reacts to languagechange when preference is system', () => {
    render(
      <PreferencesProvider>
        <I18nProvider>
          <Probe />
        </I18nProvider>
      </PreferencesProvider>,
    );

    expect(screen.getByTestId('resolved').textContent).toBe('en');

    act(() => {
      setNavigatorLanguage('zh-TW');
      window.dispatchEvent(new Event('languagechange'));
    });

    expect(screen.getByTestId('resolved').textContent).toBe('zh-CN');
  });
});
