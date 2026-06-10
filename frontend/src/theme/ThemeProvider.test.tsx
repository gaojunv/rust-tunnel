// @vitest-environment jsdom
import { act, cleanup, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ThemeProvider, useTheme } from './ThemeProvider';
import { THEME_STORAGE_KEY, type ThemePreference } from './theme';

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
  <ThemeProvider>
    <ThemeProbe />
  </ThemeProvider>,
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
  it('defaults to system and follows the current system theme', () => {
    systemMatchesDark = true;

    renderProbe();

    expect(screen.getByTestId('preference').textContent).toBe('system');
    expect(screen.getByTestId('resolvedTheme').textContent).toBe('dark');
    expect(document.documentElement.classList.contains('dark')).toBe(true);
  });

  it('persists a manual dark preference and applies the dark class', () => {
    renderProbe();

    act(() => screen.getByRole('button', { name: 'dark' }).click());

    expect(screen.getByTestId('preference').textContent).toBe('dark');
    expect(screen.getByTestId('resolvedTheme').textContent).toBe('dark');
    expect(localStorage.getItem(THEME_STORAGE_KEY)).toBe('dark');
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
