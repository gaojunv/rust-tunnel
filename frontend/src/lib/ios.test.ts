// @vitest-environment jsdom
import { describe, expect, it, afterEach } from 'vitest';
import { getIosMajorVersion, isIos26Plus, isIpadOsLike } from './ios';

// iPhone iOS 26.1（动态岛机型）：UA 带 OS 26_1 版本段
const IPHONE_IOS_26 =
  'Mozilla/5.0 (iPhone; CPU iPhone OS 26_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.1 Mobile/15E148 Safari/604.1';
const IPHONE_IOS_17 =
  'Mozilla/5.0 (iPhone; CPU iPhone OS 17_5_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1';
// iPadOS 13+ 默认桌面 UA：伪装成 Intel Mac，无 "OS xx_" iOS 段，但带 Version/ 段
const IPADOS_DESKTOP_UA =
  'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15';
const IPADOS_26_UA =
  'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.1 Safari/605.1.15';
// 极少数伪装 UA 连 Version/ 段都没有：版本不可知，保守回退
const IPADOS_NO_VERSION_UA =
  'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Safari/605.1.15';
const ANDROID_UA =
  'Mozilla/5.0 (Linux; Android 15; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36';
const DESKTOP_MAC_UA =
  'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36';

function stubNavigator(userAgent: string, platform: string, maxTouchPoints: number) {
  Object.defineProperty(window.navigator, 'userAgent', { value: userAgent, configurable: true });
  Object.defineProperty(window.navigator, 'platform', { value: platform, configurable: true });
  Object.defineProperty(window.navigator, 'maxTouchPoints', { value: maxTouchPoints, configurable: true });
}

describe('ios 版本识别', () => {
  afterEach(() => {
    // jsdom 默认 UA/平台还原（避免用例间串扰）
    stubNavigator(
      'Mozilla/5.0 (jsdom) AppleWebKit/537.36 (KHTML, like Gecko) jsdom/22.1.0',
      '',
      0,
    );
  });

  it('iPhone iOS 26.1 UA：大版本 26，isIos26Plus 为 true', () => {
    stubNavigator(IPHONE_IOS_26, 'iPhone', 5);
    expect(getIosMajorVersion()).toBe(26);
    expect(isIos26Plus()).toBe(true);
  });

  it('iPhone iOS 17 UA：大版本 17，isIos26Plus 为 false', () => {
    stubNavigator(IPHONE_IOS_17, 'iPhone', 5);
    expect(getIosMajorVersion()).toBe(17);
    expect(isIos26Plus()).toBe(false);
  });

  it('iPadOS 18 伪装 Mac UA：从 Version/ 段推断大版本 18，isIos26Plus 为 false', () => {
    stubNavigator(IPADOS_DESKTOP_UA, 'MacIntel', 5);
    expect(isIpadOsLike()).toBe(true);
    expect(getIosMajorVersion()).toBe(18);
    expect(isIos26Plus()).toBe(false);
  });

  it('iPadOS 26.1 伪装 Mac UA：从 Version/ 段推断大版本 26，isIos26Plus 为 true', () => {
    stubNavigator(IPADOS_26_UA, 'MacIntel', 5);
    expect(isIpadOsLike()).toBe(true);
    expect(getIosMajorVersion()).toBe(26);
    expect(isIos26Plus()).toBe(true);
  });

  it('iPadOS 伪装 UA 无 Version/ 段：版本不可知，保守返回 null', () => {
    stubNavigator(IPADOS_NO_VERSION_UA, 'MacIntel', 5);
    expect(isIpadOsLike()).toBe(true);
    expect(getIosMajorVersion()).toBeNull();
    expect(isIos26Plus()).toBe(false);
  });

  it('桌面 Mac UA（无多点触控）：不是 iPad，版本 null', () => {
    stubNavigator(DESKTOP_MAC_UA, 'MacIntel', 0);
    expect(isIpadOsLike()).toBe(false);
    expect(getIosMajorVersion()).toBeNull();
    expect(isIos26Plus()).toBe(false);
  });

  it('Android UA：非 iOS，版本 null', () => {
    stubNavigator(ANDROID_UA, 'Linux armv8l', 5);
    expect(isIpadOsLike()).toBe(false);
    expect(getIosMajorVersion()).toBeNull();
    expect(isIos26Plus()).toBe(false);
  });
});
