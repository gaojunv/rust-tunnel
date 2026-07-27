// @vitest-environment jsdom
import { act, cleanup, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import i18n from './index';
import { I18nProvider, useLanguagePreference } from './I18nProvider';
import { LANGUAGE_STORAGE_KEY } from './languagePreference';

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
      <I18nProvider>
        <Probe />
      </I18nProvider>,
    );

    expect(screen.getByTestId('preference').textContent).toBe('system');
    expect(screen.getByTestId('resolved').textContent).toBe('zh-CN');
    expect(i18n.language).toBe('zh-CN');
  });

  it('reads stored preference', () => {
    localStorage.setItem(LANGUAGE_STORAGE_KEY, 'en');
    setNavigatorLanguage('zh-CN');
    render(
      <I18nProvider>
        <Probe />
      </I18nProvider>,
    );

    expect(screen.getByTestId('resolved').textContent).toBe('en');
  });

  it('setPreference persists and switches i18n language', () => {
    render(
      <I18nProvider>
        <Probe />
      </I18nProvider>,
    );

    act(() => {
      screen.getByText('to-zh').click();
    });

    expect(localStorage.getItem(LANGUAGE_STORAGE_KEY)).toBe('zh-CN');
    expect(screen.getByTestId('resolved').textContent).toBe('zh-CN');
    expect(i18n.language).toBe('zh-CN');
  });

  it('falls back to system for invalid stored value', () => {
    localStorage.setItem(LANGUAGE_STORAGE_KEY, 'fr');
    render(
      <I18nProvider>
        <Probe />
      </I18nProvider>,
    );

    expect(screen.getByTestId('preference').textContent).toBe('system');
  });

  it('reacts to languagechange when preference is system', () => {
    render(
      <I18nProvider>
        <Probe />
      </I18nProvider>,
    );

    expect(screen.getByTestId('resolved').textContent).toBe('en');

    act(() => {
      setNavigatorLanguage('zh-TW');
      window.dispatchEvent(new Event('languagechange'));
    });

    expect(screen.getByTestId('resolved').textContent).toBe('zh-CN');
  });
});
