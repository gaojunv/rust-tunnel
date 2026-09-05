import { describe, it, expect } from "vitest";
import { thumbSize, thumbOffset, scrollOffsetFromThumb } from "./scrollbar-geometry";

describe("thumbSize", () => {
  it("无需滚动时返回 0", () => {
    expect(thumbSize(200, 200)).toBe(0);
    expect(thumbSize(200, 150)).toBe(0);
    expect(thumbSize(0, 100)).toBe(0);
    expect(thumbSize(100, 0)).toBe(0);
    expect(thumbSize(-10, 100)).toBe(0);
    expect(thumbSize(100, -5)).toBe(0);
  });

  it("比例正确：client^2 / scroll", () => {
    // client 200, scroll 400 -> raw = 100，默认 min 24 -> 100
    expect(thumbSize(200, 400)).toBeCloseTo(100, 6);
    // client 100, scroll 1000 -> raw = 10 -> clamp 到 24
    expect(thumbSize(100, 1000)).toBe(24);
    // 自定义 minThumb
    expect(thumbSize(100, 1000, 8)).toBeCloseTo(10, 6);
    expect(thumbSize(100, 1000, 16)).toBe(16);
  });

  it("边界：minThumb 非法时回退默认值", () => {
    expect(thumbSize(80, 1000, 0)).toBe(24);
    expect(thumbSize(80, 1000, -5)).toBe(24);
  });

  it("边界：非有限数返回 0", () => {
    expect(thumbSize(NaN, 100)).toBe(0);
    expect(thumbSize(100, Infinity)).toBe(0);
  });
});

describe("thumbOffset / scrollOffsetFromThumb", () => {
  it("无需滚动时始终 0", () => {
    expect(thumbOffset(50, 200, 200, 24)).toBe(0);
    expect(scrollOffsetFromThumb(10, 200, 200, 24)).toBe(0);
    expect(thumbOffset(10, 0, 100, 10)).toBe(0);
  });

  it("边界 clamp：scroll 越界与 thumb 越界", () => {
    const client = 200;
    const scroll = 400;
    const t = thumbSize(client, scroll); // 100
    const maxThumb = client - t; // 100
    // scroll 负数 clamp 到 0
    expect(thumbOffset(-50, client, scroll, t)).toBeCloseTo(0, 6);
    // scroll 超出 maxScroll clamp 到 maxThumb
    expect(thumbOffset(999, client, scroll, t)).toBeCloseTo(maxThumb, 6);
    // thumb 越界 clamp
    expect(scrollOffsetFromThumb(-20, client, scroll, t)).toBeCloseTo(0, 6);
    expect(scrollOffsetFromThumb(999, client, scroll, t)).toBeCloseTo(scroll - client, 6);
  });

  it("往返映射一致：offset -> thumb -> offset 恒等", () => {
    const cases: Array<[number, number]> = [
      [200, 400],
      [120, 1000],
      [300, 3000],
      [180, 500],
    ];
    for (const [client, scroll] of cases) {
      const t = thumbSize(client, scroll);
      const maxScroll = scroll - client;
      // 取若干采样点
      for (const scrollOff of [0, maxScroll * 0.25, maxScroll * 0.5, maxScroll * 0.75, maxScroll]) {
        const thumbPos = thumbOffset(scrollOff, client, scroll, t);
        const back = scrollOffsetFromThumb(thumbPos, client, scroll, t);
        expect(back).toBeCloseTo(scrollOff, 6);
      }
    }
  });

  it("往返映射一致：thumb -> offset -> thumb 恒等", () => {
    const client = 240;
    const scroll = 1200;
    const t = thumbSize(client, scroll);
    const maxThumb = client - t;
    for (const tp of [0, maxThumb * 0.33, maxThumb * 0.66, maxThumb]) {
      const off = scrollOffsetFromThumb(tp, client, scroll, t);
      const back = thumbOffset(off, client, scroll, t);
      expect(back).toBeCloseTo(tp, 6);
    }
  });

  it("非有限数返回 0", () => {
    expect(thumbOffset(NaN, 100, 200, 10)).toBe(0);
    expect(scrollOffsetFromThumb(Infinity, 100, 200, 10)).toBe(0);
  });
});
