// @vitest-environment jsdom
import { describe, expect, it, afterEach } from 'vitest';
import { cleanup, renderHook, act } from '@testing-library/react';
import { useVisualViewportHeight } from './useVisualViewportHeight';

const vvh = () => document.documentElement.style.getPropertyValue('--vvh');
const satTop = () => document.documentElement.style.getPropertyValue('--sat-top');
const satBottom = () => document.documentElement.style.getPropertyValue('--sat-bottom');

/** 模拟 visualViewport：jsdom 默认没有，用带 height 属性的 EventTarget 顶替。 */
function stubVisualViewport(height: number) {
  const vv = Object.assign(new EventTarget(), { height });
  Object.defineProperty(window, 'visualViewport', { value: vv, configurable: true });
  return vv;
}

/** jsdom 的 documentElement.clientHeight 恒为 0，stub 成指定布局高度。 */
function stubClientHeight(height: number) {
  Object.defineProperty(document.documentElement, 'clientHeight', {
    value: height,
    configurable: true,
  });
}

function stubStandalone(standalone: boolean) {
  Object.defineProperty(window, 'matchMedia', {
    configurable: true,
    value: (query: string) => ({
      matches: standalone && query === '(display-mode: standalone)',
      media: query,
      onchange: null,
      addListener: () => {},
      removeListener: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    }),
  });
}

describe('useVisualViewportHeight', () => {
  afterEach(() => {
    cleanup();
    // 每个用例独立：清掉 stub 与残留的 CSS 变量、还原 UA
    Object.defineProperty(window, 'visualViewport', { value: undefined, configurable: true });
    Object.defineProperty(window.navigator, 'userAgent', {
      configurable: true,
      value: 'Mozilla/5.0 (jsdom) AppleWebKit/537.36 (KHTML, like Gecko) jsdom/22.1.0',
    });
    stubClientHeight(0);
    ['--vvh', '--sat-top', '--sat-bottom'].forEach((p) =>
      document.documentElement.style.removeProperty(p),
    );
  });

  it('无键盘（可视高度≈布局高度）：不设置 --vvh，交由 100dvh 动态接管', () => {
    // PWA 冷启动时 innerHeight 可能报告过渡值——钉死会导致底部空白，
    // 因此平时必须移除 --vvh 而非写入当前值
    stubClientHeight(window.innerHeight);
    renderHook(() => useVisualViewportHeight());
    expect(vvh()).toBe('');
  });

  it('模拟 iOS 键盘弹出（visualViewport 明显矮于布局视口）：--vvh 钉到可视高度', () => {
    stubClientHeight(800);
    const vv = stubVisualViewport(800);
    renderHook(() => useVisualViewportHeight());
    expect(vvh()).toBe(''); // 无键盘：不钉
    act(() => {
      vv.height = 400; // 键盘弹出 → 可视高度收缩
      vv.dispatchEvent(new Event('resize'));
    });
    expect(vvh()).toBe('400px');
    act(() => {
      vv.height = 800; // 键盘收起 → 回退 100dvh
      vv.dispatchEvent(new Event('resize'));
    });
    expect(vvh()).toBe('');
  });

  it('无 visualViewport 环境：window resize 时按 innerHeight 与 clientHeight 比较', () => {
    stubClientHeight(800);
    renderHook(() => useVisualViewportHeight());
    act(() => {
      Object.defineProperty(window, 'innerHeight', { value: 500, configurable: true });
      window.dispatchEvent(new Event('resize'));
    });
    expect(vvh()).toBe('500px');
    act(() => {
      Object.defineProperty(window, 'innerHeight', { value: 800, configurable: true });
      window.dispatchEvent(new Event('resize'));
    });
    expect(vvh()).toBe('');
  });

  it('standalone 且 env(safe-area-inset-top) 解析为 0：按屏幕高度写入 --sat-top/--sat-bottom 兜底', () => {
    stubStandalone(true);
    stubClientHeight(852);
    Object.defineProperty(window, 'innerHeight', { value: 852, configurable: true });
    Object.defineProperty(window.screen, 'height', { value: 852, configurable: true });
    renderHook(() => useVisualViewportHeight());
    // jsdom 不解析 env() → 探针读出 0，走兜底：全面屏 47px + Home 指示条 34px
    expect(satTop()).toBe('47px');
    expect(satBottom()).toBe('34px');
  });

  it('iOS 26+ standalone：env=0 是系统已预留安全区的新语义（WebKit bug 301994），跳过兜底不写 --sat-*', () => {
    // iPhone iOS 26.1 UA（动态岛机 innerHeight≈screen.height 且 env=0，老逻辑必误触发）
    Object.defineProperty(window.navigator, 'userAgent', {
      configurable: true,
      value:
        'Mozilla/5.0 (iPhone; CPU iPhone OS 26_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.1 Mobile/15E148 Safari/604.1',
    });
    stubStandalone(true);
    stubClientHeight(852);
    Object.defineProperty(window, 'innerHeight', { value: 852, configurable: true });
    Object.defineProperty(window.screen, 'height', { value: 852, configurable: true });
    renderHook(() => useVisualViewportHeight());
    expect(satTop()).toBe('');
    expect(satBottom()).toBe('');
  });

  it('非 standalone（浏览器模式）：不写兜底变量', () => {
    stubStandalone(false);
    stubClientHeight(700);
    renderHook(() => useVisualViewportHeight());
    expect(satTop()).toBe('');
    expect(satBottom()).toBe('');
  });

  it('卸载后移除全部 property', () => {
    stubClientHeight(800);
    stubVisualViewport(400);
    const { unmount } = renderHook(() => useVisualViewportHeight());
    expect(vvh()).toBe('400px');
    unmount();
    expect(vvh()).toBe('');
  });
});
