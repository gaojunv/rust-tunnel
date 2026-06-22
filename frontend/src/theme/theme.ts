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
