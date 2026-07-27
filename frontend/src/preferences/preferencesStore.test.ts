import { describe, expect, it } from 'vitest';
import {
  DEFAULT_USER_PREFERENCES,
  PREFERENCES_CACHE_KEY,
  readCachedPreferences,
  writeCachedPreferences,
} from './preferencesStore';

class MemoryStorage implements Storage {
  private map = new Map<string, string>();
  get length() { return this.map.size; }
  clear() { this.map.clear(); }
  getItem(key: string) { return this.map.get(key) ?? null; }
  key(index: number) { return [...this.map.keys()][index] ?? null; }
  removeItem(key: string) { this.map.delete(key); }
  setItem(key: string, value: string) { this.map.set(key, value); }
}

describe('preferencesStore', () => {
  it('returns defaults when cache is empty', () => {
    const storage = new MemoryStorage();
    expect(readCachedPreferences(storage)).toEqual(DEFAULT_USER_PREFERENCES);
  });

  it('returns defaults when storage is undefined', () => {
    expect(readCachedPreferences(undefined)).toEqual(DEFAULT_USER_PREFERENCES);
  });

  it('round-trips preferences', () => {
    const storage = new MemoryStorage();
    const prefs = { theme: 'light' as const, language: 'en' as const, titleEffect: 'particles' as const };
    writeCachedPreferences(prefs, storage);
    expect(readCachedPreferences(storage)).toEqual(prefs);
  });

  it('merges partial cached data with defaults', () => {
    const storage = new MemoryStorage();
    storage.setItem(PREFERENCES_CACHE_KEY, JSON.stringify({ theme: 'light' }));
    const prefs = readCachedPreferences(storage);
    expect(prefs.theme).toBe('light');
    expect(prefs.language).toBe(DEFAULT_USER_PREFERENCES.language);
    expect(prefs.titleEffect).toBe(DEFAULT_USER_PREFERENCES.titleEffect);
  });

  it('rejects invalid values in cache, falls back to defaults', () => {
    const storage = new MemoryStorage();
    storage.setItem(
      PREFERENCES_CACHE_KEY,
      JSON.stringify({ theme: 'neon', language: 'en', titleEffect: 'particles' }),
    );
    const prefs = readCachedPreferences(storage);
    expect(prefs.theme).toBe(DEFAULT_USER_PREFERENCES.theme);
    expect(prefs.language).toBe('en');
    expect(prefs.titleEffect).toBe('particles');
  });

  it('returns defaults on JSON parse error', () => {
    const storage = new MemoryStorage();
    storage.setItem(PREFERENCES_CACHE_KEY, 'not-json{{{');
    expect(readCachedPreferences(storage)).toEqual(DEFAULT_USER_PREFERENCES);
  });
});
