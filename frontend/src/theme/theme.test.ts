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

  it('defaults to dark when storage is empty or invalid', () => {
    const storage = new MemoryStorage();
    expect(readStoredThemePreference(storage)).toBe('dark');

    storage.setItem(THEME_STORAGE_KEY, 'blue');
    expect(readStoredThemePreference(storage)).toBe('dark');
  });

  it('reads and writes the supported preference using the rust-tunnel key', () => {
    const storage = new MemoryStorage();

    writeStoredThemePreference('dark', storage);

    expect(storage.getItem(THEME_STORAGE_KEY)).toBe('dark');
    expect(readStoredThemePreference(storage)).toBe('dark');
  });

  it('falls back safely when storage throws', () => {
    const storage = new ThrowingStorage();

    expect(readStoredThemePreference(storage)).toBe('dark');
    expect(writeStoredThemePreference('light', storage)).toBe(false);
  });

  it('resolves system preference from the current system theme', () => {
    expect(resolveTheme('dark', 'light')).toBe('dark');
    expect(resolveTheme('light', 'dark')).toBe('light');
    expect(resolveTheme('system', 'dark')).toBe('dark');
    expect(resolveTheme('system', 'light')).toBe('light');
  });
});
