export const LANGUAGE_STORAGE_KEY = 'rust-tunnel-language';

export type LanguagePreference = 'system' | 'zh-CN' | 'en';
export type ResolvedLanguage = 'zh-CN' | 'en';

export const LANGUAGE_PREFERENCES: LanguagePreference[] = ['system', 'zh-CN', 'en'];

export const DEFAULT_LANGUAGE_PREFERENCE: LanguagePreference = 'system';

export const isLanguagePreference = (value: unknown): value is LanguagePreference =>
  typeof value === 'string' && LANGUAGE_PREFERENCES.includes(value as LanguagePreference);

export const readStoredLanguagePreference = (storage: Storage | undefined): LanguagePreference => {
  if (!storage) {
    return DEFAULT_LANGUAGE_PREFERENCE;
  }

  try {
    const value = storage.getItem(LANGUAGE_STORAGE_KEY);
    return isLanguagePreference(value) ? value : DEFAULT_LANGUAGE_PREFERENCE;
  } catch {
    return DEFAULT_LANGUAGE_PREFERENCE;
  }
};

export const writeStoredLanguagePreference = (
  preference: LanguagePreference,
  storage: Storage | undefined,
): boolean => {
  if (!storage) {
    return false;
  }

  try {
    storage.setItem(LANGUAGE_STORAGE_KEY, preference);
    return true;
  } catch {
    return false;
  }
};

export const detectSystemLanguage = (
  navigatorLanguage: string | undefined,
): ResolvedLanguage => {
  if (!navigatorLanguage) {
    return 'en';
  }

  return navigatorLanguage.toLowerCase().startsWith('zh') ? 'zh-CN' : 'en';
};

export const resolveLanguage = (
  preference: LanguagePreference,
  systemLanguage: ResolvedLanguage,
): ResolvedLanguage => {
  if (preference === 'system') {
    return systemLanguage;
  }

  return preference;
};
