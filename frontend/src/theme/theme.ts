export const THEME_STORAGE_KEY = 'rust-tunnel-theme';

export type ThemePreference = 'system' | 'light' | 'dark';
export type ResolvedTheme = 'light' | 'dark';

export const THEME_PREFERENCES: ThemePreference[] = ['system', 'light', 'dark'];

/** 未保存偏好时的默认主题：深色优先 */
export const DEFAULT_THEME_PREFERENCE: ThemePreference = 'dark';

export const isThemePreference = (value: unknown): value is ThemePreference =>
  typeof value === 'string' && THEME_PREFERENCES.includes(value as ThemePreference);

export const readStoredThemePreference = (storage: Storage | undefined): ThemePreference => {
  if (!storage) {
    return DEFAULT_THEME_PREFERENCE;
  }

  try {
    const value = storage.getItem(THEME_STORAGE_KEY);
    return isThemePreference(value) ? value : DEFAULT_THEME_PREFERENCE;
  } catch {
    return DEFAULT_THEME_PREFERENCE;
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
