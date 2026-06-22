# Dark Theme Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a full frontend dark theme that defaults to following the system theme, allows manual light/dark/system selection, persists the preference, and covers the entire management UI.

**Architecture:** Use Tailwind class-based dark mode (`darkMode: 'class'`) and a focused React theme module. The theme module owns preference persistence, system theme detection, `document.documentElement` class updates, and exposes `preference`, `resolvedTheme`, and `setPreference` through context. Components stay simple and respond via Tailwind `dark:` classes.

**Tech Stack:** React 18, TypeScript, Vite, Tailwind CSS 3, React Query v3, Recharts, Vitest + jsdom + React Testing Library for the new theme logic.

---

## File Structure

Create:

- `frontend/src/theme/theme.ts` — pure theme types, storage key, validation, safe localStorage helpers, theme resolution.
- `frontend/src/theme/theme.test.ts` — unit tests for validation, storage fallback, persistence, and resolution.
- `frontend/src/theme/ThemeProvider.tsx` — React context/provider, system theme listener, `<html class="dark">` synchronization.
- `frontend/src/theme/ThemeProvider.test.tsx` — jsdom tests for provider behavior, class synchronization, persistence, and system changes.
- `frontend/src/components/shared/ThemeToggle.tsx` — Navbar dropdown button for `system | light | dark`.

Modify:

- `frontend/package.json` and `frontend/package-lock.json` — add test script and test dev dependencies.
- `frontend/tailwind.config.js` — enable class-based dark mode at the top level.
- `frontend/src/index.css` — add color-scheme and base body transition rules.
- `frontend/src/App.tsx` — wrap the application in `ThemeProvider`; darken loading state.
- `frontend/src/components/Navbar.tsx` — add `ThemeToggle`; darken nav/menu/logout styles.
- `frontend/src/components/Dashboard.tsx` — darken application background.
- `frontend/src/components/shared/StatCard.tsx` — darken stat cards.
- `frontend/src/components/shared/ChartContainer.tsx` — darken chart containers and empty/loading text.
- `frontend/src/components/shared/TimeRangeSelector.tsx` — darken segmented buttons and datetime inputs.
- `frontend/src/components/shared/MobileBottomNav.tsx` — darken mobile navigation.
- `frontend/src/components/TrafficChart.tsx` — make Recharts axis/grid/tooltip readable in dark mode.
- Page components under `frontend/src/components/`: `Login.tsx`, `ClientList.tsx`, `ClientDetail.tsx`, `QualityPage.tsx`, `MeshPage.tsx`, `DnsPage.tsx`, `ShadowsocksPage.tsx`, `TrojanPage.tsx`, `LogsPage.tsx` — add `dark:` variants for containers, cards, tables, forms, modals, badges, empty/loading states, and hover states.

Do not modify backend files. Do not add server-side theme persistence.

---

### Task 1: Add a test harness and pure theme utilities

**Files:**
- Modify: `frontend/package.json`
- Modify: `frontend/package-lock.json`
- Create: `frontend/src/theme/theme.ts`
- Create: `frontend/src/theme/theme.test.ts`

- [ ] **Step 1: Add frontend test dependencies**

Run from the repository root:

```bash
cd frontend && npm install --save-dev vitest jsdom @testing-library/react
```

Expected:

- `frontend/package.json` gains dev dependencies for `vitest`, `jsdom`, and `@testing-library/react`.
- `frontend/package-lock.json` updates.
- No source files change yet.

- [ ] **Step 2: Add the test script**

Modify `frontend/package.json` so the `scripts` block is exactly:

```json
"scripts": {
  "dev": "vite",
  "build": "tsc && vite build",
  "lint": "eslint . --ext ts,tsx --report-unused-disable-directives --max-warnings 0",
  "test": "vitest run",
  "preview": "vite preview"
}
```

- [ ] **Step 3: Write failing tests for theme utility behavior**

Create `frontend/src/theme/theme.test.ts`:

```ts
import { describe, expect, it } from 'vitest';
import {
  THEME_STORAGE_KEY,
  isThemePreference,
  readStoredThemePreference,
  resolveTheme,
  writeStoredThemePreference,
} from './theme';

class MemoryStorage implements Storage {
  private readonly values = new Map<string, string>();
  length = 0;

  clear(): void {
    this.values.clear();
    this.length = 0;
  }

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  key(index: number): string | null {
    return Array.from(this.values.keys())[index] ?? null;
  }

  removeItem(key: string): void {
    this.values.delete(key);
    this.length = this.values.size;
  }

  setItem(key: string, value: string): void {
    this.values.set(key, value);
    this.length = this.values.size;
  }
}

class ThrowingStorage extends MemoryStorage {
  override getItem(): string | null {
    throw new Error('storage unavailable');
  }

  override setItem(): void {
    throw new Error('storage unavailable');
  }
}

describe('theme utilities', () => {
  it('recognizes only supported theme preferences', () => {
    expect(isThemePreference('system')).toBe(true);
    expect(isThemePreference('light')).toBe(true);
    expect(isThemePreference('dark')).toBe(true);
    expect(isThemePreference('blue')).toBe(false);
    expect(isThemePreference(null)).toBe(false);
  });

  it('defaults to system when storage is empty or invalid', () => {
    const storage = new MemoryStorage();
    expect(readStoredThemePreference(storage)).toBe('system');

    storage.setItem(THEME_STORAGE_KEY, 'blue');
    expect(readStoredThemePreference(storage)).toBe('system');
  });

  it('reads and writes the supported preference using the rust-tunnel key', () => {
    const storage = new MemoryStorage();

    writeStoredThemePreference('dark', storage);

    expect(storage.getItem(THEME_STORAGE_KEY)).toBe('dark');
    expect(readStoredThemePreference(storage)).toBe('dark');
  });

  it('falls back safely when storage throws', () => {
    const storage = new ThrowingStorage();

    expect(readStoredThemePreference(storage)).toBe('system');
    expect(writeStoredThemePreference('light', storage)).toBe(false);
  });

  it('resolves system preference from the current system theme', () => {
    expect(resolveTheme('dark', 'light')).toBe('dark');
    expect(resolveTheme('light', 'dark')).toBe('light');
    expect(resolveTheme('system', 'dark')).toBe('dark');
    expect(resolveTheme('system', 'light')).toBe('light');
  });
});
```

- [ ] **Step 4: Run the new tests and verify they fail**

Run:

```bash
cd frontend && npm test -- src/theme/theme.test.ts
```

Expected: FAIL because `frontend/src/theme/theme.ts` does not exist.

- [ ] **Step 5: Implement the pure theme utilities**

Create `frontend/src/theme/theme.ts`:

```ts
export const THEME_STORAGE_KEY = 'rust-tunnel-theme';

export type ThemePreference = 'system' | 'light' | 'dark';
export type ResolvedTheme = 'light' | 'dark';

export const THEME_PREFERENCES: ThemePreference[] = ['system', 'light', 'dark'];

export const isThemePreference = (value: unknown): value is ThemePreference =>
  typeof value === 'string' && THEME_PREFERENCES.includes(value as ThemePreference);

export const readStoredThemePreference = (storage: Storage | undefined): ThemePreference => {
  if (!storage) {
    return 'system';
  }

  try {
    const value = storage.getItem(THEME_STORAGE_KEY);
    return isThemePreference(value) ? value : 'system';
  } catch {
    return 'system';
  }
};

export const writeStoredThemePreference = (
  preference: ThemePreference,
  storage: Storage | undefined,
): boolean => {
  if (!storage) {
    return false;
  }

  try {
    storage.setItem(THEME_STORAGE_KEY, preference);
    return true;
  } catch {
    return false;
  }
};

export const resolveTheme = (
  preference: ThemePreference,
  systemTheme: ResolvedTheme,
): ResolvedTheme => {
  if (preference === 'system') {
    return systemTheme;
  }

  return preference;
};
```

- [ ] **Step 6: Run the tests and verify they pass**

Run:

```bash
cd frontend && npm test -- src/theme/theme.test.ts
```

Expected: PASS for all tests in `theme.test.ts`.

- [ ] **Step 7: Run lint and build for this small slice**

Run:

```bash
cd frontend && npm run lint && npm run build
```

Expected: both commands pass.

- [ ] **Step 8: Commit Task 1**

Run:

```bash
git add frontend/package.json frontend/package-lock.json frontend/src/theme/theme.ts frontend/src/theme/theme.test.ts
git commit -m "test: add theme utility tests" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Add ThemeProvider with system tracking and `<html class="dark">` synchronization

**Files:**
- Create: `frontend/src/theme/ThemeProvider.tsx`
- Create: `frontend/src/theme/ThemeProvider.test.tsx`

- [ ] **Step 1: Write failing provider tests**

Create `frontend/src/theme/ThemeProvider.test.tsx`:

```tsx
// @vitest-environment jsdom
import { act, render, screen } from '@testing-library/react';
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
```

- [ ] **Step 2: Run provider tests and verify they fail**

Run:

```bash
cd frontend && npm test -- src/theme/ThemeProvider.test.tsx
```

Expected: FAIL because `ThemeProvider.tsx` does not exist.

- [ ] **Step 3: Implement ThemeProvider**

Create `frontend/src/theme/ThemeProvider.tsx`:

```tsx
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react';
import {
  readStoredThemePreference,
  resolveTheme,
  writeStoredThemePreference,
  type ResolvedTheme,
  type ThemePreference,
} from './theme';

interface ThemeContextValue {
  preference: ThemePreference;
  resolvedTheme: ResolvedTheme;
  setPreference: (preference: ThemePreference) => void;
}

const ThemeContext = createContext<ThemeContextValue | undefined>(undefined);

const getStorage = (): Storage | undefined => {
  try {
    return window.localStorage;
  } catch {
    return undefined;
  }
};

const getSystemTheme = (): ResolvedTheme => {
  if (typeof window === 'undefined' || !window.matchMedia) {
    return 'light';
  }

  return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
};

const applyResolvedTheme = (resolvedTheme: ResolvedTheme) => {
  document.documentElement.classList.toggle('dark', resolvedTheme === 'dark');
};

interface ThemeProviderProps {
  children: ReactNode;
}

export const ThemeProvider = ({ children }: ThemeProviderProps) => {
  const [preference, setPreferenceState] = useState<ThemePreference>(() =>
    readStoredThemePreference(getStorage()),
  );
  const [systemTheme, setSystemTheme] = useState<ResolvedTheme>(() => getSystemTheme());

  const resolvedTheme = resolveTheme(preference, systemTheme);

  useEffect(() => {
    applyResolvedTheme(resolvedTheme);
  }, [resolvedTheme]);

  useEffect(() => {
    if (typeof window === 'undefined' || !window.matchMedia) {
      return undefined;
    }

    const mediaQuery = window.matchMedia('(prefers-color-scheme: dark)');
    const handleChange = (event: MediaQueryListEvent) => {
      setSystemTheme(event.matches ? 'dark' : 'light');
    };

    setSystemTheme(mediaQuery.matches ? 'dark' : 'light');
    mediaQuery.addEventListener('change', handleChange);

    return () => {
      mediaQuery.removeEventListener('change', handleChange);
    };
  }, []);

  const setPreference = useCallback((nextPreference: ThemePreference) => {
    setPreferenceState(nextPreference);
    writeStoredThemePreference(nextPreference, getStorage());
  }, []);

  const value = useMemo(
    () => ({ preference, resolvedTheme, setPreference }),
    [preference, resolvedTheme, setPreference],
  );

  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>;
};

export const useTheme = (): ThemeContextValue => {
  const context = useContext(ThemeContext);

  if (!context) {
    throw new Error('useTheme must be used within a ThemeProvider');
  }

  return context;
};
```

- [ ] **Step 4: Run provider tests and verify they pass**

Run:

```bash
cd frontend && npm test -- src/theme/ThemeProvider.test.tsx
```

Expected: PASS for all provider tests.

- [ ] **Step 5: Run the full frontend test suite**

Run:

```bash
cd frontend && npm test
```

Expected: PASS for `theme.test.ts` and `ThemeProvider.test.tsx`.

- [ ] **Step 6: Run lint and build**

Run:

```bash
cd frontend && npm run lint && npm run build
```

Expected: both commands pass.

- [ ] **Step 7: Commit Task 2**

Run:

```bash
git add frontend/src/theme/ThemeProvider.tsx frontend/src/theme/ThemeProvider.test.tsx
git commit -m "feat: add theme provider" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Enable Tailwind dark mode and integrate ThemeProvider at the app root

**Files:**
- Modify: `frontend/tailwind.config.js`
- Modify: `frontend/src/index.css`
- Modify: `frontend/src/App.tsx`

- [ ] **Step 1: Add class-based dark mode to Tailwind**

Modify `frontend/tailwind.config.js` so it is exactly:

```js
/** @type {import('tailwindcss').Config} */
export default {
  darkMode: 'class',
  content: [
    './index.html',
    './src/**/*.{js,ts,jsx,tsx}',
  ],
  theme: {
    extend: {},
  },
  plugins: [],
}
```

- [ ] **Step 2: Add global color-scheme support**

Modify `frontend/src/index.css` so it remains Tailwind-first and includes these base rules after the three Tailwind directives:

```css
@tailwind base;
@tailwind components;
@tailwind utilities;

@layer base {
  html {
    color-scheme: light;
  }

  html.dark {
    color-scheme: dark;
  }

  body {
    @apply bg-gray-50 text-gray-900 transition-colors duration-150 dark:bg-slate-900 dark:text-slate-100;
  }
}
```

- [ ] **Step 3: Wrap the app in ThemeProvider and darken loading state**

Modify `frontend/src/App.tsx`:

1. Add the import:

```ts
import { ThemeProvider } from './theme/ThemeProvider';
```

2. Replace the loading return with:

```tsx
return (
  <ThemeProvider>
    <div className="min-h-screen flex items-center justify-center bg-gray-50 dark:bg-slate-900">
      <div className="text-center">
        <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-blue-600 mx-auto mb-4" />
        <p className="text-gray-600 dark:text-slate-300">Loading...</p>
      </div>
    </div>
  </ThemeProvider>
);
```

3. Replace the authenticated dashboard return with:

```tsx
return (
  <ThemeProvider>
    <QueryClientProvider client={queryClient}>
      <Dashboard onLogout={handleLogout} />
    </QueryClientProvider>
  </ThemeProvider>
);
```

4. Replace the login return with:

```tsx
return (
  <ThemeProvider>
    <QueryClientProvider client={queryClient}>
      <Login onLogin={handleLogin} />
    </QueryClientProvider>
  </ThemeProvider>
);
```

- [ ] **Step 4: Run tests, lint, and build**

Run:

```bash
cd frontend && npm test && npm run lint && npm run build
```

Expected: all commands pass.

- [ ] **Step 5: Commit Task 3**

Run:

```bash
git add frontend/tailwind.config.js frontend/src/index.css frontend/src/App.tsx
git commit -m "feat: enable frontend dark mode root" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: Add the three-state Navbar theme switcher

**Files:**
- Create: `frontend/src/components/shared/ThemeToggle.tsx`
- Modify: `frontend/src/components/Navbar.tsx`

- [ ] **Step 1: Create ThemeToggle**

Create `frontend/src/components/shared/ThemeToggle.tsx`:

```tsx
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { useTheme } from '../../theme/ThemeProvider';
import type { ThemePreference } from '../../theme/theme';

const OPTIONS: Array<{ value: ThemePreference; label: string; description: string }> = [
  { value: 'system', label: '跟随系统', description: '根据系统主题自动切换' },
  { value: 'light', label: '浅色', description: '固定使用浅色主题' },
  { value: 'dark', label: '深色', description: '固定使用深色主题' },
];

const SystemIcon = () => (
  <svg className="h-5 w-5" fill="none" viewBox="0 0 24 24" stroke="currentColor" aria-hidden="true">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9.75 17L9 20l-1 1h8l-1-1-.75-3M3 13h18M5 17h14a2 2 0 002-2V5a2 2 0 00-2-2H5a2 2 0 00-2 2v10a2 2 0 002 2z" />
  </svg>
);

const SunIcon = () => (
  <svg className="h-5 w-5" fill="none" viewBox="0 0 24 24" stroke="currentColor" aria-hidden="true">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 3v1m0 16v1m9-9h-1M4 12H3m15.364 6.364l-.707-.707M6.343 6.343l-.707-.707m12.728 0l-.707.707M6.343 17.657l-.707.707M16 12a4 4 0 11-8 0 4 4 0 018 0z" />
  </svg>
);

const MoonIcon = () => (
  <svg className="h-5 w-5" fill="none" viewBox="0 0 24 24" stroke="currentColor" aria-hidden="true">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M20.354 15.354A9 9 0 018.646 3.646 9.003 9.003 0 0012 21a9.003 9.003 0 008.354-5.646z" />
  </svg>
);

const CheckIcon = () => (
  <svg className="h-4 w-4" fill="none" viewBox="0 0 24 24" stroke="currentColor" aria-hidden="true">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
  </svg>
);

const iconByPreference: Record<ThemePreference, ReactNode> = {
  system: <SystemIcon />,
  light: <SunIcon />,
  dark: <MoonIcon />,
};

export const ThemeToggle = () => {
  const { preference, resolvedTheme, setPreference } = useTheme();
  const [open, setOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) {
      return undefined;
    }

    const handlePointerDown = (event: PointerEvent) => {
      if (!containerRef.current?.contains(event.target as Node)) {
        setOpen(false);
      }
    };

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        setOpen(false);
      }
    };

    document.addEventListener('pointerdown', handlePointerDown);
    document.addEventListener('keydown', handleKeyDown);

    return () => {
      document.removeEventListener('pointerdown', handlePointerDown);
      document.removeEventListener('keydown', handleKeyDown);
    };
  }, [open]);

  return (
    <div className="relative" ref={containerRef}>
      <button
        type="button"
        className="inline-flex items-center justify-center rounded-md p-2 text-gray-300 transition-colors hover:bg-gray-700 hover:text-white focus:outline-none focus:ring-2 focus:ring-blue-500 focus:ring-offset-2 focus:ring-offset-gray-800 dark:text-slate-200 dark:hover:bg-slate-700 dark:focus:ring-offset-slate-900"
        aria-label={`切换主题，当前为${preference === 'system' ? `跟随系统（${resolvedTheme === 'dark' ? '深色' : '浅色'}）` : preference === 'dark' ? '深色' : '浅色'}`}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((current) => !current)}
      >
        {iconByPreference[preference]}
      </button>

      {open && (
        <div className="absolute right-0 z-50 mt-2 w-48 overflow-hidden rounded-md border border-gray-200 bg-white py-1 shadow-lg dark:border-slate-700 dark:bg-slate-800">
          {OPTIONS.map((option) => {
            const selected = option.value === preference;
            return (
              <button
                key={option.value}
                type="button"
                role="menuitemradio"
                aria-checked={selected}
                className={`flex w-full items-start gap-3 px-3 py-2 text-left text-sm transition-colors ${
                  selected
                    ? 'bg-blue-50 text-blue-700 dark:bg-blue-500/10 dark:text-blue-300'
                    : 'text-gray-700 hover:bg-gray-50 dark:text-slate-200 dark:hover:bg-slate-700'
                }`}
                onClick={() => {
                  setPreference(option.value);
                  setOpen(false);
                }}
              >
                <span className="mt-0.5 text-gray-500 dark:text-slate-400">{iconByPreference[option.value]}</span>
                <span className="min-w-0 flex-1">
                  <span className="block font-medium">{option.label}</span>
                  <span className="block text-xs text-gray-500 dark:text-slate-400">{option.description}</span>
                </span>
                {selected && <span className="mt-0.5 text-blue-600 dark:text-blue-300"><CheckIcon /></span>}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
};
```

- [ ] **Step 2: Modify Navbar to use ThemeToggle**

In `frontend/src/components/Navbar.tsx`, add:

```ts
import { ThemeToggle } from './shared/ThemeToggle';
```

Replace the root `<nav>` class with:

```tsx
<nav className="bg-gray-800 dark:bg-slate-900 dark:border-b dark:border-slate-800">
```

Replace every inactive tab class fragment:

```tsx
'text-gray-300 hover:bg-gray-700 hover:text-white'
```

with:

```tsx
'text-gray-300 hover:bg-gray-700 hover:text-white dark:text-slate-300 dark:hover:bg-slate-800 dark:hover:text-white'
```

Replace every active tab class fragment:

```tsx
'bg-gray-900 text-white'
```

with:

```tsx
'bg-gray-900 text-white dark:bg-slate-800'
```

Replace the right-side container:

```tsx
<div className="ml-4 flex items-center md:ml-6">
```

with:

```tsx
<div className="ml-4 flex items-center gap-3 md:ml-6">
  <ThemeToggle />
```

Then keep the existing logout button after `<ThemeToggle />`, and replace its class with:

```tsx
className="bg-gray-700 hover:bg-gray-600 text-white px-4 py-2 rounded-md text-sm font-medium disabled:opacity-50 dark:bg-slate-700 dark:hover:bg-slate-600"
```

- [ ] **Step 3: Run tests, lint, and build**

Run:

```bash
cd frontend && npm test && npm run lint && npm run build
```

Expected: all commands pass.

- [ ] **Step 4: Commit Task 4**

Run:

```bash
git add frontend/src/components/shared/ThemeToggle.tsx frontend/src/components/Navbar.tsx
git commit -m "feat: add theme selector" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: Darken shared layout primitives and Recharts chrome

**Files:**
- Modify: `frontend/src/components/Dashboard.tsx`
- Modify: `frontend/src/components/shared/StatCard.tsx`
- Modify: `frontend/src/components/shared/ChartContainer.tsx`
- Modify: `frontend/src/components/shared/TimeRangeSelector.tsx`
- Modify: `frontend/src/components/shared/MobileBottomNav.tsx`
- Modify: `frontend/src/components/TrafficChart.tsx`

- [ ] **Step 1: Darken the Dashboard shell**

In `frontend/src/components/Dashboard.tsx`, replace:

```tsx
<div className="min-h-screen bg-gray-100">
```

with:

```tsx
<div className="min-h-screen bg-gray-100 text-gray-900 transition-colors duration-150 dark:bg-slate-900 dark:text-slate-100">
```

- [ ] **Step 2: Darken StatCard**

In `frontend/src/components/shared/StatCard.tsx`, replace the card container class:

```tsx
className="bg-white overflow-hidden shadow rounded-lg p-4 sm:p-6"
```

with:

```tsx
className="bg-white overflow-hidden shadow rounded-lg p-4 sm:p-6 transition-colors dark:bg-slate-800 dark:shadow-slate-950/20"
```

Replace the label class:

```tsx
className="text-sm font-medium text-gray-500 truncate"
```

with:

```tsx
className="text-sm font-medium text-gray-500 truncate dark:text-slate-400"
```

Replace the value class expression:

```tsx
<dd className={`text-lg font-semibold ${valueColor || 'text-gray-900'}`}>{value}</dd>
```

with:

```tsx
<dd className={`text-lg font-semibold ${valueColor || 'text-gray-900 dark:text-slate-100'}`}>{value}</dd>
```

- [ ] **Step 3: Darken ChartContainer**

In `frontend/src/components/shared/ChartContainer.tsx`, replace:

```tsx
<div className={`bg-white p-4 sm:p-6 rounded-lg shadow ${className}`}>
```

with:

```tsx
<div className={`bg-white p-4 sm:p-6 rounded-lg shadow transition-colors dark:bg-slate-800 dark:shadow-slate-950/20 ${className}`}>
```

Replace:

```tsx
<h3 className="text-lg font-medium text-gray-900">{title}</h3>
```

with:

```tsx
<h3 className="text-lg font-medium text-gray-900 dark:text-slate-100">{title}</h3>
```

Replace:

```tsx
<p className="text-gray-500 text-center py-8">No data available</p>
```

with:

```tsx
<p className="text-gray-500 text-center py-8 dark:text-slate-400">No data available</p>
```

- [ ] **Step 4: Darken TimeRangeSelector**

In `frontend/src/components/shared/TimeRangeSelector.tsx`, update inactive preset button classes from:

```tsx
'bg-white text-gray-700 border-gray-300 hover:bg-gray-50'
```

to:

```tsx
'bg-white text-gray-700 border-gray-300 hover:bg-gray-50 dark:bg-slate-800 dark:text-slate-200 dark:border-slate-600 dark:hover:bg-slate-700'
```

Apply the same replacement to the inactive `Custom` button class.

Replace both datetime input classes:

```tsx
className="px-2 py-1 text-xs border border-gray-300 rounded-md w-full sm:w-auto"
```

with:

```tsx
className="px-2 py-1 text-xs border border-gray-300 rounded-md w-full bg-white text-gray-900 sm:w-auto dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
```

Replace:

```tsx
<span className="text-xs text-gray-400">-</span>
```

with:

```tsx
<span className="text-xs text-gray-400 dark:text-slate-500">-</span>
```

- [ ] **Step 5: Darken MobileBottomNav**

In `frontend/src/components/shared/MobileBottomNav.tsx`, apply these class rules:

- Root fixed container: add `dark:border-slate-700 dark:bg-slate-900` to the existing white/background border class.
- Active item text/background: add `dark:bg-blue-500/10 dark:text-blue-300`.
- Inactive item text/hover: add `dark:text-slate-400 dark:hover:bg-slate-800 dark:hover:text-slate-200`.

After the edit, run this grep to confirm the file has dark variants:

```bash
grep -n "dark:" frontend/src/components/shared/MobileBottomNav.tsx
```

Expected: output includes at least `dark:bg-slate-900`, `dark:border-slate-700`, and `dark:text-slate-400`.

- [ ] **Step 6: Make TrafficChart readable in dark mode**

In `frontend/src/components/TrafficChart.tsx`, import `useTheme`:

```ts
import { useTheme } from '../theme/ThemeProvider';
```

Inside `TrafficChart`, after `isSmallScreen`, add:

```ts
const { resolvedTheme } = useTheme();
const isDark = resolvedTheme === 'dark';
const axisColor = isDark ? '#cbd5e1' : '#4b5563';
const gridColor = isDark ? '#334155' : '#e5e7eb';
const tooltipStyle = {
  backgroundColor: isDark ? '#0f172a' : '#ffffff',
  border: `1px solid ${isDark ? '#334155' : '#e5e7eb'}`,
  color: isDark ? '#e2e8f0' : '#111827',
};
const legendStyle = { color: axisColor, fontSize: isSmallScreen ? '10px' : '12px' };
```

Replace:

```tsx
<CartesianGrid strokeDasharray="3 3" />
```

with:

```tsx
<CartesianGrid strokeDasharray="3 3" stroke={gridColor} />
```

Update `XAxis` and `YAxis` tick props to include fill:

```tsx
tick={{ fontSize: isSmallScreen ? 9 : 12, fill: axisColor }}
```

Add `stroke={axisColor}` to both `XAxis` and `YAxis`.

Update `Tooltip` to include:

```tsx
contentStyle={tooltipStyle}
labelStyle={{ color: tooltipStyle.color }}
itemStyle={{ color: tooltipStyle.color }}
```

Replace the `Legend` wrapper style with:

```tsx
wrapperStyle={legendStyle}
```

- [ ] **Step 7: Run tests, lint, and build**

Run:

```bash
cd frontend && npm test && npm run lint && npm run build
```

Expected: all commands pass.

- [ ] **Step 8: Commit Task 5**

Run:

```bash
git add frontend/src/components/Dashboard.tsx frontend/src/components/shared/StatCard.tsx frontend/src/components/shared/ChartContainer.tsx frontend/src/components/shared/TimeRangeSelector.tsx frontend/src/components/shared/MobileBottomNav.tsx frontend/src/components/TrafficChart.tsx
git commit -m "feat: darken shared frontend components" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 6: Darken all page-level UI surfaces

**Files:**
- Modify: `frontend/src/components/Login.tsx`
- Modify: `frontend/src/components/ClientList.tsx`
- Modify: `frontend/src/components/ClientDetail.tsx`
- Modify: `frontend/src/components/QualityPage.tsx`
- Modify: `frontend/src/components/MeshPage.tsx`
- Modify: `frontend/src/components/DnsPage.tsx`
- Modify: `frontend/src/components/ShadowsocksPage.tsx`
- Modify: `frontend/src/components/TrojanPage.tsx`
- Modify: `frontend/src/components/LogsPage.tsx`

- [ ] **Step 1: Apply the shared dark-mode replacement table**

For each file in this task, apply these exact class additions wherever the matching light class appears. Keep the existing light class and append the dark variant.

| Existing class | Add |
| --- | --- |
| `bg-white` | `dark:bg-slate-800` |
| `bg-gray-50` | `dark:bg-slate-900` for page backgrounds; `dark:bg-slate-700/50` for nested panels/table headers |
| `bg-gray-100` | `dark:bg-slate-900` |
| `text-gray-900` | `dark:text-slate-100` |
| `text-gray-800` | `dark:text-slate-100` |
| `text-gray-700` | `dark:text-slate-200` |
| `text-gray-600` | `dark:text-slate-300` |
| `text-gray-500` | `dark:text-slate-400` |
| `text-gray-400` | `dark:text-slate-500` |
| `border-gray-300` | `dark:border-slate-600` |
| `border-gray-200` | `dark:border-slate-700` |
| `divide-gray-200` | `dark:divide-slate-700` |
| `hover:bg-gray-50` | `dark:hover:bg-slate-700/50` |
| `placeholder-gray-500` | `dark:placeholder-slate-500` |
| input/select `text-gray-900` | also add `dark:bg-slate-900 dark:text-slate-100` |
| `shadow` or `shadow-lg` on cards/modals | add `dark:shadow-slate-950/20` |

Do not change status colors semantically. For action/status text, add dark readable hover variants:

| Existing action class | Add |
| --- | --- |
| `text-blue-600 hover:text-blue-900` | `dark:text-blue-400 dark:hover:text-blue-300` |
| `text-red-600 hover:text-red-900` | `dark:text-red-400 dark:hover:text-red-300` |
| `text-green-600` | `dark:text-green-400` |
| `text-yellow-600` | `dark:text-yellow-400` |

- [ ] **Step 2: Darken Login exactly**

In `frontend/src/components/Login.tsx`, ensure these final classes exist:

- Root div:

```tsx
className="min-h-screen flex items-center justify-center bg-gray-50 py-12 px-4 sm:px-6 lg:px-8 dark:bg-slate-900"
```

- Title:

```tsx
className="mt-6 text-center text-3xl font-extrabold text-gray-900 dark:text-slate-100"
```

- Subtitle:

```tsx
className="mt-2 text-center text-sm text-gray-600 dark:text-slate-300"
```

- Password input:

```tsx
className="appearance-none rounded-none relative block w-full px-3 py-2 border border-gray-300 placeholder-gray-500 text-gray-900 rounded-t-md focus:outline-none focus:ring-blue-500 focus:border-blue-500 focus:z-10 sm:text-sm dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100 dark:placeholder-slate-500"
```

- Error:

```tsx
className="text-red-600 text-sm text-center dark:text-red-400"
```

- Submit button:

```tsx
className="group relative w-full flex justify-center py-2 px-4 border border-transparent text-sm font-medium rounded-md text-white bg-blue-600 hover:bg-blue-700 focus:outline-none focus:ring-2 focus:ring-offset-2 focus:ring-blue-500 disabled:opacity-50 dark:focus:ring-offset-slate-900"
```

- [ ] **Step 3: Verify no page file lacks dark variants**

Run:

```bash
grep -L "dark:" \
  frontend/src/components/Login.tsx \
  frontend/src/components/ClientList.tsx \
  frontend/src/components/ClientDetail.tsx \
  frontend/src/components/QualityPage.tsx \
  frontend/src/components/MeshPage.tsx \
  frontend/src/components/DnsPage.tsx \
  frontend/src/components/ShadowsocksPage.tsx \
  frontend/src/components/TrojanPage.tsx \
  frontend/src/components/LogsPage.tsx
```

Expected: no output.

- [ ] **Step 4: Search for remaining light-only gray surfaces**

Run:

```bash
grep -RInE "bg-white|bg-gray-50|bg-gray-100|text-gray-900|border-gray-200|divide-gray-200|placeholder-gray-500" frontend/src --include='*.tsx'
```

For each match, confirm the same JSX class string includes an appropriate `dark:` class. If a match is a status color or a false positive, leave it unchanged and note it in the task log before committing.

- [ ] **Step 5: Run tests, lint, and build**

Run:

```bash
cd frontend && npm test && npm run lint && npm run build
```

Expected: all commands pass.

- [ ] **Step 6: Commit Task 6**

Run:

```bash
git add frontend/src/components/Login.tsx frontend/src/components/ClientList.tsx frontend/src/components/ClientDetail.tsx frontend/src/components/QualityPage.tsx frontend/src/components/MeshPage.tsx frontend/src/components/DnsPage.tsx frontend/src/components/ShadowsocksPage.tsx frontend/src/components/TrojanPage.tsx frontend/src/components/LogsPage.tsx
git commit -m "feat: darken frontend pages" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 7: Final verification and cleanup

**Files:**
- Inspect: `frontend/src/**/*.{ts,tsx,css}`
- Inspect: `frontend/tailwind.config.js`
- Inspect: `frontend/package.json`

- [ ] **Step 1: Run the full automated checks**

Run:

```bash
cd frontend && npm test && npm run lint && npm run build
```

Expected:

- `npm test` passes all theme tests.
- `npm run lint` exits with code 0 and no warnings because the script uses `--max-warnings 0`.
- `npm run build` exits with code 0 and produces `frontend/dist/`.

- [ ] **Step 2: Run a final source scan for missed theme surfaces**

Run:

```bash
grep -RInE "bg-white|bg-gray-50|bg-gray-100|text-gray-900|text-gray-700|border-gray-200|border-gray-300|divide-gray-200|hover:bg-gray-50|placeholder-gray-500" frontend/src --include='*.tsx'
```

Expected: remaining matches are acceptable only when their surrounding class string contains a corresponding `dark:` variant.

- [ ] **Step 3: Manually verify browser behavior**

Run:

```bash
cd frontend && npm run dev
```

Open the Vite URL shown in the terminal. Verify:

1. With no `localStorage['rust-tunnel-theme']`, the UI follows the system theme.
2. Selecting “深色” adds `dark` to `<html>` and stores `rust-tunnel-theme=dark`.
3. Selecting “浅色” removes `dark` from `<html>` and stores `rust-tunnel-theme=light`.
4. Selecting “跟随系统” stores `rust-tunnel-theme=system` and follows `prefers-color-scheme` again.
5. Refreshing the page preserves the selected preference.
6. Login, Navbar, Dashboard, ClientList, TrafficChart, QualityPage, MeshPage, DnsPage, ShadowsocksPage, TrojanPage, LogsPage, ClientDetail modal, and MobileBottomNav are readable in dark mode.

- [ ] **Step 4: Check git status**

Run:

```bash
git status --short
```

Expected: only intentional files are modified, plus `frontend/dist/` remains ignored.

- [ ] **Step 5: Commit any final polish**

If Step 2 or Step 3 required final fixes, commit them:

```bash
git add frontend
git commit -m "fix: polish dark theme coverage" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

If no files changed, skip this commit and record that no final polish was needed.

---

## Self-Review

Spec coverage:

- Default follows system: Task 1 resolves `system`, Task 2 tracks `matchMedia`, Task 7 verifies manually.
- Manual three-state selection: Task 4 creates `ThemeToggle` with `system | light | dark`.
- Persistence: Task 1 storage helpers and Task 2 provider tests use `rust-tunnel-theme`.
- Neutral slate/blue-gray dark style: Tasks 5 and 6 use `slate-*` classes and preserve semantic status colors.
- Full management UI coverage: Task 6 covers all page-level components; Task 7 scans and manually verifies remaining surfaces.
- No backend changes: file structure and tasks are frontend-only.
- No new UI library/global state manager: implementation uses React context and Tailwind only.

Placeholder scan:

- The plan contains no unfinished-marker language or deferred-work markers.
- The only manual judgment step is the final visual verification, and it includes an explicit page checklist and expected behavior.

Type consistency:

- `ThemePreference`, `ResolvedTheme`, `preference`, `resolvedTheme`, and `setPreference` are defined in Tasks 1–2 and reused consistently in Tasks 4–5.
- Storage key is consistently `rust-tunnel-theme`.
- `ThemeProvider` import path is consistently `./theme/ThemeProvider` from `App.tsx` and `../theme/ThemeProvider` from component files.
