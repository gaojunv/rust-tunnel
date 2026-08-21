import { useEffect } from 'react';
import { isIos26Plus } from '../lib/ios';

/**
 * 把可视视口高度写入 :root 的 --vvh CSS 变量（仅键盘弹出等可视区收缩场景）。
 *
 * 背景：iOS Safari 弹出软键盘时 layout viewport 不缩小（100dvh 不变），
 * 依赖视口高度的 /agent 布局底部 sticky 输入框会被键盘遮挡。
 * visualViewport.height 会随键盘弹出收缩，此时把布局高度钉到可视高度
 * （h-[var(--vvh,100dvh)]），输入框贴键盘上沿。
 *
 * iOS 26+ 不做任何键盘干预（update() 直接 return，不写 --vvh）：iOS 26 浏览器
 * 自身已能把聚焦的输入框滚动到键盘上方，--vvh 钉高会改变 /agent 根容器
 * （h-[var(--vvh,100dvh)]）高度、引发整个 flex 页面重排上移，与浏览器的滚动
 * 叠加后表现为「点输入框整个页面往上跑」（iPhone 16 Pro PWA 实测反馈）。
 * iOS ≤25 与其他浏览器仍靠本机制兜底。
 *
 * 平时（无键盘）主动移除 --vvh 而非写入当前值：iOS PWA 冷启动时
 * visualViewport/innerHeight 会经历过渡态、报告非最终值，且 standalone
 * 模式没有地址栏伸缩、之后可能再不触发 resize——若把启动瞬间的值钉死，
 * 容器比屏幕矮一截，页面底部露出空白。移除后布局由 100dvh 动态计算，
 * 永远跟随真实视口。
 *
 * standalone 安全区兜底（仅 iOS ≤25 等老系统）：部分 iOS 版本在老 web-app
 * 模式（apple-mobile-web-app-capable 优先于 manifest）下 env(safe-area-inset-top)
 * 恒为 0，但内容又确实延伸到状态栏/刘海后面 → 用探针实测 env 解析值，
 * 为 0 且视口占满整屏（innerHeight≈screen.height 证明延伸生效）时按屏幕
 * 高度估算 --sat-top/--sat-bottom，配合 CSS max(env(...), var(--sat-*)) 兜底。
 * iOS 26.1+ 的 env=0 是新语义（系统已预留安全区），不适用此兜底，直接跳过。
 */
export function useVisualViewportHeight() {
  useEffect(() => {
    const root = document.documentElement;
    const vv = window.visualViewport;

    const update = () => {
      // iOS 26+ 不做键盘干预：浏览器自身会把聚焦输入框滚动到键盘上方，
      // 这里再钉 --vvh 会改根容器高度、与之叠加造成页面整体上移（实测反馈）
      if (isIos26Plus()) return;
      const visual = vv?.height ?? window.innerHeight;
      // 布局视口高度：键盘弹出时不变（iOS），PWA 全屏下 = 屏幕高
      const layout = document.documentElement.clientHeight;
      // 阈值 100px 区分「键盘弹出」（键盘高 ~300px）与滚动补偿/渲染抖动
      if (visual < layout - 100) {
        root.style.setProperty('--vvh', `${Math.round(visual)}px`);
      } else {
        root.style.removeProperty('--vvh');
      }
    };

    const applyStandaloneSafeAreaFallback = () => {
      // iOS 26.1+ WebKit 回归（bug 301994）：standalone 下内容不再延伸到
      // 状态栏/Home 指示条后面，env(safe-area-inset-*) 返回 0 是「新语义」而
      // 非老 web-app 模式的解析失败——系统已预留安全区、布局视口正确。若仍按
      // 老逻辑写入 --sat-top/--sat-bottom，会把 Header 多垫 47px、底部栏多垫
      // 34px，造成双重留白（iPhone 动态岛机 innerHeight≈screen.height 且
      // env=0，必然误触发）。故 iOS 26+ 直接跳过本兜底。
      if (isIos26Plus()) return;
      const standalone =
        window.matchMedia?.('(display-mode: standalone)').matches === true ||
        (navigator as Navigator & { standalone?: boolean }).standalone === true;
      if (!standalone) return;
      const probe = document.createElement('div');
      probe.style.cssText =
        'position:fixed;top:0;visibility:hidden;pointer-events:none;' +
        'padding-top:env(safe-area-inset-top);padding-bottom:env(safe-area-inset-bottom)';
      document.body.appendChild(probe);
      const cs = getComputedStyle(probe);
      const top = parseFloat(cs.paddingTop) || 0;
      const bottom = parseFloat(cs.paddingBottom) || 0;
      probe.remove();
      // innerHeight≈screen.height 证明内容延伸到了状态栏/Home 指示条后面，
      // 此时 env 读出 0 才是 bug（无刘海老机型 env 本就该是 0，不能误伤）
      const fullBleed = window.innerHeight >= window.screen.height - 1;
      if (!fullBleed) return;
      if (top === 0) {
        // 刘海/动态岛机（全面屏 ≥812pt）状态栏区 47px，老机型 20px。
        // 取 47 统一值：env=0 的多为旧系统老 web-app 模式（刘海机为主）；
        // 动态岛机（59px）env 通常正常，即便触发 47 也能避开状态栏文字本体。
        root.style.setProperty('--sat-top', window.screen.height >= 812 ? '47px' : '20px');
      }
      if (bottom === 0 && window.screen.height >= 812) {
        // 全面屏 Home 指示条 34px
        root.style.setProperty('--sat-bottom', '34px');
      }
    };

    update();
    applyStandaloneSafeAreaFallback();
    // 冷启动过渡态修正：下一帧 + 短延迟各重采一次
    const raf = requestAnimationFrame(update);
    const timer = setTimeout(update, 300);

    vv?.addEventListener('resize', update);
    vv?.addEventListener('scroll', update);
    window.addEventListener('resize', update);
    window.addEventListener('orientationchange', update);
    return () => {
      cancelAnimationFrame(raf);
      clearTimeout(timer);
      vv?.removeEventListener('resize', update);
      vv?.removeEventListener('scroll', update);
      window.removeEventListener('resize', update);
      window.removeEventListener('orientationchange', update);
      root.style.removeProperty('--vvh');
      root.style.removeProperty('--sat-top');
      root.style.removeProperty('--sat-bottom');
    };
  }, []);
}
