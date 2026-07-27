// @vitest-environment jsdom
import { act, cleanup, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import * as apiModule from '../api/preferences';
import { PreferencesProvider, usePreferences } from './PreferencesProvider';
import { DEFAULT_USER_PREFERENCES, PREFERENCES_CACHE_KEY } from './preferencesStore';

vi.mock('../api/preferences', () => ({
  fetchPreferences: vi.fn(),
  updatePreferences: vi.fn(),
}));

function Probe() {
  const { prefs, setPreference } = usePreferences();
  return (
    <div>
      <div data-testid="theme">{prefs.theme}</div>
      <div data-testid="language">{prefs.language}</div>
      <div data-testid="titleEffect">{prefs.titleEffect}</div>
      <button onClick={() => setPreference('theme', 'light')}>set-light</button>
      <button onClick={() => setPreference('titleEffect', 'particles')}>set-particles</button>
    </div>
  );
}

describe('PreferencesProvider', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.clearAllMocks();
    vi.mocked(apiModule.fetchPreferences).mockResolvedValue({
      theme: 'dark',
      language: 'system',
      title_effect: 'grid-wave',
    });
    vi.mocked(apiModule.updatePreferences).mockResolvedValue(undefined);
  });

  afterEach(() => {
    cleanup();
  });

  it('renders with defaults when cache is empty', async () => {
    render(
      <PreferencesProvider>
        <Probe />
      </PreferencesProvider>,
    );
    expect(screen.getByTestId('theme').textContent).toBe(DEFAULT_USER_PREFERENCES.theme);
    expect(screen.getByTestId('language').textContent).toBe(DEFAULT_USER_PREFERENCES.language);
    expect(screen.getByTestId('titleEffect').textContent).toBe(DEFAULT_USER_PREFERENCES.titleEffect);
  });

  it('hydrates from localStorage cache before server response', async () => {
    localStorage.setItem(
      PREFERENCES_CACHE_KEY,
      JSON.stringify({ theme: 'light', language: 'en', titleEffect: 'none' }),
    );
    // 延迟服务器响应，确保先用缓存渲染
    vi.mocked(apiModule.fetchPreferences).mockImplementation(
      () => new Promise(() => {}), // never resolves
    );
    render(
      <PreferencesProvider>
        <Probe />
      </PreferencesProvider>,
    );
    expect(screen.getByTestId('theme').textContent).toBe('light');
    expect(screen.getByTestId('language').textContent).toBe('en');
    expect(screen.getByTestId('titleEffect').textContent).toBe('none');
  });

  it('overrides local cache with server response', async () => {
    localStorage.setItem(
      PREFERENCES_CACHE_KEY,
      JSON.stringify({ theme: 'light', language: 'en', titleEffect: 'none' }),
    );
    vi.mocked(apiModule.fetchPreferences).mockResolvedValue({
      theme: 'dark',
      language: 'zh-CN',
      title_effect: 'particles',
    });
    render(
      <PreferencesProvider>
        <Probe />
      </PreferencesProvider>,
    );
    await waitFor(() => {
      expect(screen.getByTestId('theme').textContent).toBe('dark');
    });
    expect(screen.getByTestId('language').textContent).toBe('zh-CN');
    expect(screen.getByTestId('titleEffect').textContent).toBe('particles');
  });

  it('setPreference optimistically updates local state and calls API', async () => {
    render(
      <PreferencesProvider>
        <Probe />
      </PreferencesProvider>,
    );
    await waitFor(() => expect(apiModule.fetchPreferences).toHaveBeenCalled());

    act(() => {
      screen.getByRole('button', { name: 'set-light' }).click();
    });
    expect(screen.getByTestId('theme').textContent).toBe('light');
    await waitFor(() => {
      expect(apiModule.updatePreferences).toHaveBeenCalledWith({
        theme: 'light',
        language: 'system',
        title_effect: 'grid-wave',
      });
    });
    // localStorage 缓存也更新
    const cached = JSON.parse(localStorage.getItem(PREFERENCES_CACHE_KEY)!);
    expect(cached.theme).toBe('light');
  });

  it('rolls back when API update fails', async () => {
    vi.mocked(apiModule.updatePreferences).mockRejectedValue(new Error('network'));
    render(
      <PreferencesProvider>
        <Probe />
      </PreferencesProvider>,
    );
    await waitFor(() => expect(apiModule.fetchPreferences).toHaveBeenCalled());

    act(() => {
      screen.getByRole('button', { name: 'set-light' }).click();
    });
    expect(screen.getByTestId('theme').textContent).toBe('light');
    await waitFor(() => {
      expect(screen.getByTestId('theme').textContent).toBe('dark');
    });
  });

  it('snake_cases keys when calling API', async () => {
    render(
      <PreferencesProvider>
        <Probe />
      </PreferencesProvider>,
    );
    await waitFor(() => expect(apiModule.fetchPreferences).toHaveBeenCalled());

    act(() => {
      screen.getByRole('button', { name: 'set-particles' }).click();
    });
    await waitFor(() => {
      expect(apiModule.updatePreferences).toHaveBeenCalledWith(
        expect.objectContaining({ title_effect: 'particles' }),
      );
    });
  });
});
