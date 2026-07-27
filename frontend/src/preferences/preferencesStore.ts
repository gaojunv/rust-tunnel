import { isLanguagePreference, type LanguagePreference } from '../i18n/languagePreference';
import { isThemePreference, type ThemePreference } from '../theme/theme';
import {
  DEFAULT_TITLE_EFFECT,
  isTitleEffectPreference,
  type TitleEffectPreference,
} from '../effects/titleEffectPreference';

export const PREFERENCES_CACHE_KEY = 'rust-tunnel-preferences-cache';

export interface UserPreferences {
  theme: ThemePreference;
  language: LanguagePreference;
  titleEffect: TitleEffectPreference;
}

export const DEFAULT_USER_PREFERENCES: UserPreferences = {
  theme: 'dark',
  language: 'system',
  titleEffect: DEFAULT_TITLE_EFFECT,
};

export function readCachedPreferences(storage: Storage | undefined): UserPreferences {
  if (!storage) return DEFAULT_USER_PREFERENCES;
  let raw: string | null = null;
  try {
    raw = storage.getItem(PREFERENCES_CACHE_KEY);
  } catch {
    return DEFAULT_USER_PREFERENCES;
  }
  if (!raw) return DEFAULT_USER_PREFERENCES;
  try {
    const parsed = JSON.parse(raw) as Partial<Record<keyof UserPreferences, unknown>>;
    return {
      theme: isThemePreference(parsed.theme) ? parsed.theme : DEFAULT_USER_PREFERENCES.theme,
      language: isLanguagePreference(parsed.language)
        ? parsed.language
        : DEFAULT_USER_PREFERENCES.language,
      titleEffect: isTitleEffectPreference(parsed.titleEffect)
        ? parsed.titleEffect
        : DEFAULT_USER_PREFERENCES.titleEffect,
    };
  } catch {
    return DEFAULT_USER_PREFERENCES;
  }
}

export function writeCachedPreferences(
  prefs: UserPreferences,
  storage: Storage | undefined,
): boolean {
  if (!storage) return false;
  try {
    storage.setItem(PREFERENCES_CACHE_KEY, JSON.stringify(prefs));
    return true;
  } catch {
    return false;
  }
}
