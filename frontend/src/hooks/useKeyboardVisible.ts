import { useEffect, useRef } from 'react';
import type { MutableRefObject } from 'react';

/**
 * 软键盘弹起判定，返回 ref（供 effect 内读取，避免弹收引发组件重渲染）。
 *
 * 判定口径与 useVisualViewportHeight 一致：visualViewport.height 比布局视口
 * （documentElement.clientHeight）矮 100px 以上视为键盘弹起——iOS 键盘高 ~300px，
 * 100px 阈值可区分键盘与滚动补偿/渲染抖动。监听 visualViewport resize；
 * jsdom/旧浏览器无 visualViewport 时恒为 false（不可知就当无键盘，不阻塞既有行为）。
 *
 * 用途：iOS 26 起 Safari/PWA 聚焦输入框时浏览器已自行滚动页面使焦点可见，
 * 键盘弹起期间的程序化贴底滚动（scrollIntoView）会与浏览器滚动打架，
 * 造成「输入时页面向上跳」——调用方在键盘弹起时跳过即可。
 */
export function useKeyboardVisible(): MutableRefObject<boolean> {
  const visibleRef = useRef(false);

  useEffect(() => {
    const vv = window.visualViewport;
    // 无 visualViewport 环境（jsdom/旧浏览器）：无法判定，保持 false
    if (!vv) return;

    const update = () => {
      const layout = document.documentElement.clientHeight;
      visibleRef.current = vv.height < layout - 100;
    };

    update();
    vv.addEventListener('resize', update);
    return () => {
      vv.removeEventListener('resize', update);
      visibleRef.current = false;
    };
  }, []);

  return visibleRef;
}
