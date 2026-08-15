// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  safeLocalStorageGet,
  safeLocalStorageRemove,
  safeLocalStorageSet,
} from './safeStorage';

describe('safeLocalStorage', () => {
  afterEach(() => {
    // 用 safe 包装清理：某些用例把 localStorage 换成会抛异常的替身，直接 clear 会炸
    safeLocalStorageRemove('k');
    vi.unstubAllGlobals();
  });

  it('delegates to localStorage when available', () => {
    safeLocalStorageSet('k', 'v');
    expect(safeLocalStorageGet('k')).toBe('v');
    expect(localStorage.getItem('k')).toBe('v');
    safeLocalStorageRemove('k');
    expect(safeLocalStorageGet('k')).toBeNull();
  });

  it('does not throw when localStorage access throws (privacy mode / disabled)', () => {
    // 模拟隐私模式：getItem/setItem/removeItem 抛 SecurityError
    const boom = () => {
      throw new Error('SecurityError: The operation is insecure.');
    };
    const originalDesc = Object.getOwnPropertyDescriptor(window, 'localStorage');
    Object.defineProperty(window, 'localStorage', { configurable: true, get: boom });
    try {
      // 读失败 → null，写失败 → 静默丢弃（调用方按「无持久化」处理，不崩 UI）
      expect(safeLocalStorageGet('k')).toBeNull();
      expect(() => safeLocalStorageSet('k', 'v')).not.toThrow();
      expect(() => safeLocalStorageRemove('k')).not.toThrow();
    } finally {
      // 恢复真实 localStorage，避免污染后续用例
      if (originalDesc) {
        Object.defineProperty(window, 'localStorage', originalDesc);
      } else {
        delete (window as unknown as { localStorage?: unknown }).localStorage;
      }
    }
  });

  it('is a no-op when window is undefined (SSR)', () => {
    const saved = globalThis.window;
    // @ts-expect-error 模拟 SSR 环境无 window
    delete globalThis.window;
    try {
      expect(safeLocalStorageGet('k')).toBeNull();
      expect(() => safeLocalStorageSet('k', 'v')).not.toThrow();
      expect(() => safeLocalStorageRemove('k')).not.toThrow();
    } finally {
      globalThis.window = saved;
    }
  });
});
