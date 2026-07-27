// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';
import {
  DEFAULT_LANGUAGE_PREFERENCE,
  LANGUAGE_STORAGE_KEY,
  detectSystemLanguage,
  isLanguagePreference,
  readStoredLanguagePreference,
  resolveLanguage,
  writeStoredLanguagePreference,
} from './languagePreference';

describe('isLanguagePreference', () => {
  it.each(['system', 'zh-CN', 'en'])('accepts %s', (value) => {
    expect(isLanguagePreference(value)).toBe(true);
  });

  it.each(['fr', 'zh', '', 1, null, undefined])('rejects %s', (value) => {
    expect(isLanguagePreference(value)).toBe(false);
  });
});

describe('readStoredLanguagePreference', () => {
  it('returns default when storage is undefined', () => {
    expect(readStoredLanguagePreference(undefined)).toBe(DEFAULT_LANGUAGE_PREFERENCE);
  });

  it('returns stored valid preference', () => {
    localStorage.setItem(LANGUAGE_STORAGE_KEY, 'zh-CN');
    expect(readStoredLanguagePreference(window.localStorage)).toBe('zh-CN');
  });

  it('returns default for invalid stored value', () => {
    localStorage.setItem(LANGUAGE_STORAGE_KEY, 'fr');
    expect(readStoredLanguagePreference(window.localStorage)).toBe(DEFAULT_LANGUAGE_PREFERENCE);
  });
});

describe('writeStoredLanguagePreference', () => {
  it('persists preference', () => {
    expect(writeStoredLanguagePreference('en', window.localStorage)).toBe(true);
    expect(localStorage.getItem(LANGUAGE_STORAGE_KEY)).toBe('en');
  });

  it('returns false when storage is undefined', () => {
    expect(writeStoredLanguagePreference('en', undefined)).toBe(false);
  });
});

describe('detectSystemLanguage', () => {
  it.each(['zh-CN', 'zh-TW', 'zh-HK', 'zh'])('maps %s to zh-CN', (lang) => {
    expect(detectSystemLanguage(lang)).toBe('zh-CN');
  });

  it.each(['en-US', 'en', 'ja-JP', 'fr-FR'])('maps %s to en', (lang) => {
    expect(detectSystemLanguage(lang)).toBe('en');
  });

  it('maps undefined to en', () => {
    expect(detectSystemLanguage(undefined)).toBe('en');
  });
});

describe('resolveLanguage', () => {
  it('returns system language when preference is system', () => {
    expect(resolveLanguage('system', 'zh-CN')).toBe('zh-CN');
    expect(resolveLanguage('system', 'en')).toBe('en');
  });

  it('returns explicit preference', () => {
    expect(resolveLanguage('en', 'zh-CN')).toBe('en');
    expect(resolveLanguage('zh-CN', 'en')).toBe('zh-CN');
  });
});
