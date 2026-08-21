/**
 * iOS 版本识别工具。
 *
 * 用途：iOS 26.1 起 WebKit 回归（bug 301994）改变了 PWA standalone 的安全区语义
 * ——内容不再延伸到状态栏/Home 指示条后面，env(safe-area-inset-*) 返回 0 但系统
 * 已预留该区域。针对 iOS 26+ 与老版本需要走不同的兜底分支，故需版本判定。
 *
 * 注意 iPadOS 13+ 默认把 UA 伪装成 Mac（platform='MacIntel' 且 maxTouchPoints>1），
 * UA 不含 "OS xx_" 段；但带 `Version/xx.x` 段（WebKit 内核版本跟随系统），
 * getIosMajorVersion 据此推断版本，缺 Version/ 段时保守返回 null（回退老逻辑）。
 */

/** 是否为 iPadOS 伪装成 Mac 的桌面 UA（MacIntel + 多点触控）。 */
export function isIpadOsLike(): boolean {
  return (
    typeof navigator !== 'undefined' &&
    navigator.platform === 'MacIntel' &&
    navigator.maxTouchPoints > 1
  );
}

/**
 * 提取 iOS 大版本号；非 iOS 或版本不可知时返回 null。
 * iPhone/iPod/iPad 真实 UA 形如 `... OS 18_1 like Mac OS X ...`，取 OS 后的数字段。
 */
export function getIosMajorVersion(): number | null {
  if (typeof navigator === 'undefined') return null;
  const ua = navigator.userAgent;
  // 仅 iPhone/iPod/iPad 字样的 UA 才带真实 iOS 版本段
  if (/iPhone|iPod|iPad/.test(ua)) {
    const m = ua.match(/OS (\d+)[_ ]/);
    if (!m) return null;
    const major = parseInt(m[1], 10);
    return Number.isNaN(major) ? null : major;
  }
  // iPadOS 13+ 默认「请求桌面网站」：UA 伪装成 Mac（不含版本段），但 WebKit 内核
  // 版本跟随 Safari 26。从 UA 的 Version/xx.x 段推断——MacIntel+触摸 的 UA 里
  // Version/ 即 iPadOS 版本，判定可信。无 Version/ 段则保守返回 null（回退老逻辑）。
  if (isIpadOsLike()) {
    const m = ua.match(/Version\/(\d+)/);
    if (!m) return null;
    const major = parseInt(m[1], 10);
    return Number.isNaN(major) ? null : major;
  }
  return null;
}

/** iOS 26 及以上（含无法区分小版本的 26.x）。版本不可知时保守返回 false。 */
export function isIos26Plus(): boolean {
  const v = getIosMajorVersion();
  return v !== null && v >= 26;
}
