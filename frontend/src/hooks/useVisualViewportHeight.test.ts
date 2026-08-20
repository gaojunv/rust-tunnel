// @vitest-environment jsdom
import { describe, expect, it, afterEach } from 'vitest';
import { cleanup, renderHook, act } from '@testing-library/react';
import { useVisualViewportHeight } from './useVisualViewportHeight';

const vvh = () => document.documentElement.style.getPropertyValue('--vvh');

/** 模拟 visualViewport：jsdom 默认没有，用带 height 属性的 EventTarget 顶替。 */
function stubVisualViewport(height: number) {
  const vv = Object.assign(new EventTarget(), { height });
  Object.defineProperty(window, 'visualViewport', { value: vv, configurable: true });
  return vv;
}

describe('useVisualViewportHeight', () => {
  afterEach(() => {
    cleanup();
    // 每个用例独立：清掉 stub 与残留的 CSS 变量
    Object.defineProperty(window, 'visualViewport', { value: undefined, configurable: true });
    document.documentElement.style.removeProperty('--vvh');
  });

  it('jsdom 无 visualViewport：回退 window.innerHeight', () => {
    renderHook(() => useVisualViewportHeight());
    expect(vvh()).toBe(`${Math.round(window.innerHeight)}px`);
  });

  it('无 visualViewport 时 window resize 事件更新 --vvh', () => {
    renderHook(() => useVisualViewportHeight());
    act(() => {
      Object.defineProperty(window, 'innerHeight', { value: 555, configurable: true });
      window.dispatchEvent(new Event('resize'));
    });
    expect(vvh()).toBe('555px');
  });

  it('有 visualViewport 时用其高度，resize 事件同步更新（模拟 iOS 键盘弹出）', () => {
    const vv = stubVisualViewport(800);
    renderHook(() => useVisualViewportHeight());
    expect(vvh()).toBe('800px');
    // 键盘弹出 → 可视高度收缩
    act(() => {
      vv.height = 400;
      vv.dispatchEvent(new Event('resize'));
    });
    expect(vvh()).toBe('400px');
  });

  it('卸载后移除 --vvh property', () => {
    const { unmount } = renderHook(() => useVisualViewportHeight());
    expect(vvh()).not.toBe('');
    unmount();
    expect(vvh()).toBe('');
  });
});
