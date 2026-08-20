import { useEffect } from 'react';

/**
 * 把可视视口高度写入 :root 的 --vvh CSS 变量。
 *
 * 背景：iOS Safari 弹出软键盘时 layout viewport 不缩小（100dvh 不变），
 * 依赖视口高度的 /agent 布局底部 sticky 输入框会被键盘遮挡。
 * visualViewport.height 会随键盘弹出收缩，监听它的 resize/scroll 事件，
 * 让布局高度（h-[var(--vvh,100dvh)]）跟随可视区域，输入框贴键盘上沿。
 *
 * 无 visualViewport 的环境（jsdom / 老浏览器）回退 window.innerHeight +
 * window resize 监听，保证变量始终有值；组件卸载时移除该 property。
 */
export function useVisualViewportHeight() {
  useEffect(() => {
    const root = document.documentElement;
    const vv = window.visualViewport;
    const update = () => {
      const h = Math.round(vv ? vv.height : window.innerHeight);
      root.style.setProperty('--vvh', `${h}px`);
    };
    update();
    if (vv) {
      // 键盘弹出/收起、双指缩放都会触发 resize；iOS 上键盘弹出时页面
      // 可能被顶上去（visual viewport 偏移），scroll 事件补偿该场景
      vv.addEventListener('resize', update);
      vv.addEventListener('scroll', update);
      return () => {
        vv.removeEventListener('resize', update);
        vv.removeEventListener('scroll', update);
        root.style.removeProperty('--vvh');
      };
    }
    window.addEventListener('resize', update);
    return () => {
      window.removeEventListener('resize', update);
      root.style.removeProperty('--vvh');
    };
  }, []);
}
