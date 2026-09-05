/**
 * @vitest-environment jsdom
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  PANEL_LIMITS,
  COLLAPSE_THRESHOLD,
  clampPanelWidth,
  resolveDragWidth,
  defaultRightCollapsed,
  loadPanelLayout,
  savePanelLayout,
  type PanelLayout,
} from "./panel-layout";

describe("clampPanelWidth", () => {
  it("小于最小值时钳制到 min", () => {
    expect(clampPanelWidth("left", 10)).toBe(PANEL_LIMITS.left.min);
    expect(clampPanelWidth("right", 10)).toBe(PANEL_LIMITS.right.min);
  });

  it("大于最大值时钳制到 max", () => {
    expect(clampPanelWidth("left", 9999)).toBe(PANEL_LIMITS.left.max);
    expect(clampPanelWidth("right", 9999)).toBe(PANEL_LIMITS.right.max);
  });

  it("范围内原样返回", () => {
    expect(clampPanelWidth("left", 250)).toBe(250);
    expect(clampPanelWidth("right", 400)).toBe(400);
  });

  it("边界值恰好等于 min/max", () => {
    expect(clampPanelWidth("left", PANEL_LIMITS.left.min)).toBe(PANEL_LIMITS.left.min);
    expect(clampPanelWidth("left", PANEL_LIMITS.left.max)).toBe(PANEL_LIMITS.left.max);
    expect(clampPanelWidth("right", PANEL_LIMITS.right.min)).toBe(PANEL_LIMITS.right.min);
    expect(clampPanelWidth("right", PANEL_LIMITS.right.max)).toBe(PANEL_LIMITS.right.max);
  });

  it("非有限值返回 defaultWidth", () => {
    expect(clampPanelWidth("left", NaN)).toBe(PANEL_LIMITS.left.defaultWidth);
    expect(clampPanelWidth("left", Infinity)).toBe(PANEL_LIMITS.left.defaultWidth);
    expect(clampPanelWidth("right", -Infinity)).toBe(PANEL_LIMITS.right.defaultWidth);
  });
});

describe("resolveDragWidth 吸附阈值", () => {
  it("宽度小于阈值则 collapsed:true", () => {
    const r = resolveDragWidth("left", COLLAPSE_THRESHOLD - 1);
    expect(r.collapsed).toBe(true);
    const r2 = resolveDragWidth("right", 0);
    expect(r2.collapsed).toBe(true);
  });

  it("宽度等于阈值不吸附", () => {
    const r = resolveDragWidth("left", COLLAPSE_THRESHOLD);
    expect(r.collapsed).toBe(false);
    expect(r.width).toBe(PANEL_LIMITS.left.min);
  });

  it("阈值以上且小于 min 时 width 钳制到 min 且不吸附", () => {
    // 阈值 120，left min 200，所以 [120,199] 应钳制到 200 且不吸附
    const r = resolveDragWidth("left", 150);
    expect(r.collapsed).toBe(false);
    expect(r.width).toBe(PANEL_LIMITS.left.min);
  });

  it("正常范围返回钳制值且不吸附", () => {
    const r = resolveDragWidth("left", 300);
    expect(r).toEqual({ width: 300, collapsed: false });
    const r2 = resolveDragWidth("right", 700);
    expect(r2).toEqual({ width: PANEL_LIMITS.right.max, collapsed: false });
  });

  it("小于阈值时 width 仍为 clamp 后的值", () => {
    const r = resolveDragWidth("left", 50);
    expect(r.collapsed).toBe(true);
    expect(r.width).toBe(PANEL_LIMITS.left.min);
  });
});

describe("defaultRightCollapsed", () => {
  it("窄屏默认折叠", () => {
    expect(defaultRightCollapsed(1279)).toBe(true);
    expect(defaultRightCollapsed(0)).toBe(true);
  });

  it("1280 及以上不折叠", () => {
    expect(defaultRightCollapsed(1280)).toBe(false);
    expect(defaultRightCollapsed(1920)).toBe(false);
  });
});

describe("loadPanelLayout / savePanelLayout", () => {
  const key = "wiki.layout.test";

  beforeEach(() => {
    window.localStorage.removeItem(key);
    window.localStorage.removeItem("wiki.layout.v1");
  });

  afterEach(() => {
    window.localStorage.removeItem(key);
    window.localStorage.removeItem("wiki.layout.v1");
    vi.restoreAllMocks();
  });

  it("无持久化时返回默认值（右栏按视口）", () => {
    // jsdom 默认 innerWidth=1024 (<1280) → 右栏折叠
    const layout = loadPanelLayout(key);
    expect(layout.leftCollapsed).toBe(false);
    // 实现会按 window.innerWidth 决定
    expect(layout.rightCollapsed).toBe(true);
  });

  it("坏 JSON 降级为默认", () => {
    window.localStorage.setItem(key, "{not-json");
    const layout = loadPanelLayout(key);
    expect(layout.leftWidth).toBe(PANEL_LIMITS.left.defaultWidth);
    expect(layout.rightWidth).toBe(PANEL_LIMITS.right.defaultWidth);
  });

  it("缺字段时保留默认值并钳制已给字段", () => {
    window.localStorage.setItem(key, JSON.stringify({ leftWidth: 9999 }));
    const layout = loadPanelLayout(key);
    expect(layout.leftWidth).toBe(PANEL_LIMITS.left.max);
    expect(layout.rightWidth).toBe(PANEL_LIMITS.right.defaultWidth);
    expect(layout.leftCollapsed).toBe(false);
  });

  it("非对象/数组时降级", () => {
    window.localStorage.setItem(key, JSON.stringify([1, 2, 3]));
    expect(loadPanelLayout(key).leftWidth).toBe(PANEL_LIMITS.left.defaultWidth);
    window.localStorage.setItem(key, JSON.stringify(null));
    expect(loadPanelLayout(key).leftWidth).toBe(PANEL_LIMITS.left.defaultWidth);
  });

  it("错误类型字段被忽略", () => {
    window.localStorage.setItem(
      key,
      JSON.stringify({ leftWidth: "300", rightWidth: null, leftCollapsed: "true", rightCollapsed: 1 }),
    );
    const layout = loadPanelLayout(key);
    expect(layout.leftWidth).toBe(PANEL_LIMITS.left.defaultWidth);
    expect(layout.rightCollapsed).toBe(false);
  });

  it("save/load 往返", () => {
    const src: PanelLayout = { leftWidth: 250, rightWidth: 400, leftCollapsed: true, rightCollapsed: true };
    savePanelLayout(src, key);
    const loaded = loadPanelLayout(key);
    expect(loaded).toEqual(src);
  });

  it("save 时钳制宽度", () => {
    const src: PanelLayout = { leftWidth: 9999, rightWidth: -10, leftCollapsed: false, rightCollapsed: false };
    savePanelLayout(src, key);
    const loaded = loadPanelLayout(key);
    expect(loaded.leftWidth).toBe(PANEL_LIMITS.left.max);
    expect(loaded.rightWidth).toBe(PANEL_LIMITS.right.min);
  });

  it("用户显式持久化后不再受视口影响", () => {
    const src: PanelLayout = {
      leftWidth: 300,
      rightWidth: 320,
      leftCollapsed: false,
      rightCollapsed: false,
    };
    savePanelLayout(src, key);
    // 即使视口很窄，已持久化的 false 仍被尊重
    Object.defineProperty(window, "innerWidth", { value: 800, configurable: true });
    const loaded = loadPanelLayout(key);
    expect(loaded.rightCollapsed).toBe(false);
  });

  it("localStorage 异常时 load 降级且 save 静默", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(loadPanelLayout(key).leftWidth).toBe(PANEL_LIMITS.left.defaultWidth);
    vi.restoreAllMocks();
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("quota");
    });
    expect(() =>
      savePanelLayout({ leftWidth: 300, rightWidth: 320, leftCollapsed: false, rightCollapsed: false }, key),
    ).not.toThrow();
  });
});
