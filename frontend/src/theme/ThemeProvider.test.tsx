// @vitest-environment jsdom
import { act, cleanup, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { PreferencesProvider } from '../preferences/PreferencesProvider';
import { PREFERENCES_CACHE_KEY } from '../preferences/preferencesStore';
import { ThemeProvider, useTheme } from './ThemeProvider';
import { type ThemePreference } from './theme';

vi.mock('../api/preferences', () => ({
  fetchPreferences: vi.fn().mockResolvedValue({
    theme: 'dark',
    language: 'system',
    title_effect: 'grid-wave',
  }),
  updatePreferences: vi.fn().mockResolvedValue(undefined),
}));

const listeners = new Set<(event: MediaQueryListEvent) => void>();
let systemMatchesDark = false;

const installMatchMedia = () => {
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    value: vi.fn().mockImplementation((query: string) => ({
      matches: systemMatchesDark,
      media: query,
      onchange: null,
      addEventListener: (_event: string, listener: (event: MediaQueryListEvent) => void) => {
        listeners.add(listener);
      },
      removeEventListener: (_event: string, listener: (event: MediaQueryListEvent) => void) => {
        listeners.delete(listener);
      },
      addListener: vi.fn(),
      removeListener: vi.fn(),
      dispatchEvent: vi.fn(),
    })),
  });
};

const emitSystemTheme = (matches: boolean) => {
  systemMatchesDark = matches;
  const event = { matches } as MediaQueryListEvent;
  listeners.forEach((listener) => listener(event));
};

const ThemeProbe = () => {
  const { preference, resolvedTheme, setPreference } = useTheme();
  const set = (value: ThemePreference) => () => setPreference(value);

  return (
    <div>
      <p data-testid="preference">{preference}</p>
      <p data-testid="resolvedTheme">{resolvedTheme}</p>
      <button type="button" onClick={set('system')}>system</button>
      <button type="button" onClick={set('light')}>light</button>
      <button type="button" onClick={set('dark')}>dark</button>
    </div>
  );
};

const renderProbe = () => render(
  <PreferencesProvider>
    <ThemeProvider>
      <ThemeProbe />
    </ThemeProvider>
  </PreferencesProvider>,
);

beforeEach(() => {
  systemMatchesDark = false;
  listeners.clear();
  localStorage.clear();
  document.documentElement.className = '';
  installMatchMedia();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe('ThemeProvider', () => {
  it('defaults to dark regardless of the current system theme', () => {
    renderProbe();

    expect(screen.getByTestId('preference').textContent).toBe('dark');
    expect(screen.getByTestId('resolvedTheme').textContent).toBe('dark');
    expect(document.documentElement.classList.contains('dark')).toBe(true);
  });

  it('persists a manual dark preference and applies the dark class', () => {
    renderProbe();

    act(() => screen.getByRole('button', { name: 'dark' }).click());

    expect(screen.getByTestId('preference').textContent).toBe('dark');
    expect(screen.getByTestId('resolvedTheme').textContent).toBe('dark');
    const stored = JSON.parse(localStorage.getItem(PREFERENCES_CACHE_KEY) ?? '{}');
    expect(stored.theme).toBe('dark');
    expect(document.documentElement.classList.contains('dark')).toBe(true);
  });

  it('does not follow system changes while manually pinned to light', () => {
    renderProbe();

    act(() => screen.getByRole('button', { name: 'light' }).click());
    act(() => emitSystemTheme(true));

    expect(screen.getByTestId('preference').textContent).toBe('light');
    expect(screen.getByTestId('resolvedTheme').textContent).toBe('light');
    expect(document.documentElement.classList.contains('dark')).toBe(false);
  });

  it('resumes live system tracking after switching back to system', () => {
    renderProbe();

    act(() => screen.getByRole('button', { name: 'dark' }).click());
    act(() => screen.getByRole('button', { name: 'system' }).click());
    act(() => emitSystemTheme(true));

    expect(screen.getByTestId('preference').textContent).toBe('system');
    expect(screen.getByTestId('resolvedTheme').textContent).toBe('dark');
    expect(document.documentElement.classList.contains('dark')).toBe(true);
  });
});
