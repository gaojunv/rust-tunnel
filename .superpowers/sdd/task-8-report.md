# Task 8 Report: Mount PreferencesProvider at App Root

## Changes Made

### 1. `frontend/src/App.tsx`
- Added `import { PreferencesProvider } from './preferences/PreferencesProvider'`
- Wrapped existing provider tree (`ThemeProvider` > `I18nProvider` > `QueryClientProvider`) with `<PreferencesProvider>`

### 2. `frontend/src/components/layout/Header.test.tsx`
- Added `PreferencesProvider` wrapper around `ThemeProvider` and `I18nProvider`
- Added `vi.mock` for `../../api/preferences` to prevent real XHR (which hangs in jsdom between tests)
- Added `vi.mock` for `../dataflow/DataFlowBackground.tsx` to prevent Three.js WebGL context creation (unavailable in jsdom)
- Added `window.matchMedia` polyfill for Logo component
- Added explicit `cleanup()` in `afterEach` (vitest does not auto-cleanup in this setup)
- Changed Chinese test to use `PREFERENCES_CACHE_KEY` instead of legacy `rust-tunnel-language` key

### 3. `frontend/src/components/settings/AppearanceTab.test.tsx`
- Added `PreferencesProvider` wrapper around `I18nProvider`
- Added `vi.mock` for `../../api/preferences` with dynamic `readCachedPreferences()` call so mock data respects localStorage cache
- Added explicit `cleanup()` in `afterEach`
- Changed Chinese test to use `PREFERENCES_CACHE_KEY` instead of legacy `rust-tunnel-language` key

## Key Technical Details

- **XHR hang**: `PreferencesProvider` calls `fetchPreferences()` on mount via `useEffect`, which makes an XHR request. In jsdom, this XHR keeps the Node.js event loop alive. When two tests share a file, vitest waits for pending async operations between tests, causing an indefinite hang. Solution: `vi.mock` the `api/preferences` module to resolve immediately.
- **Dynamic mock**: The mock factory reads from `/preferences/preferencesStore.readCachedPreferences()` at call-time to respect the localStorage cache that each test sets up.
- **DataFlowBackground**: Uses Three.js WebGL which is unavailable in jsdom. Must be mocked (with `.tsx` extension).
- **Cleanup**: `@testing-library/react` auto-cleanup does not fire in this vitest setup; explicit `cleanup()` in `afterEach` is required.

## Verification

- TypeScript: `npx tsc --noEmit` passes
- Tests: All 96 tests pass across 17 files (0 failed)
- Commit: `7a46a60`
