export type PanelSide = "left" | "right";

export interface PanelLayout {
  leftWidth: number;
  rightWidth: number;
  leftCollapsed: boolean;
  rightCollapsed: boolean;
}

export const PANEL_LIMITS: Record<PanelSide, { min: number; max: number; defaultWidth: number }> = {
  left: { min: 200, max: 480, defaultWidth: 300 },
  right: { min: 240, max: 560, defaultWidth: 320 },
};

export const COLLAPSE_THRESHOLD = 120;

const STORAGE_KEY_DEFAULT = "wiki.layout.v1";

const DEFAULT_LAYOUT: PanelLayout = {
  leftWidth: PANEL_LIMITS.left.defaultWidth,
  rightWidth: PANEL_LIMITS.right.defaultWidth,
  leftCollapsed: false,
  rightCollapsed: false,
};

export function clampPanelWidth(side: PanelSide, w: number): number {
  const lim = PANEL_LIMITS[side];
  if (!Number.isFinite(w)) return lim.defaultWidth;
  return Math.min(lim.max, Math.max(lim.min, w));
}

export function resolveDragWidth(side: PanelSide, w: number): { width: number; collapsed: boolean } {
  if (w < COLLAPSE_THRESHOLD) {
    // 吸附隐藏：width 返回 clamp 后的值，但调用方应保持原宽度不变以便恢复
    return { width: clampPanelWidth(side, w), collapsed: true };
  }
  return { width: clampPanelWidth(side, w), collapsed: false };
}

export function defaultRightCollapsed(viewportWidth: number): boolean {
  // 窄屏（<1280）默认折叠右栏
  return viewportWidth < 1280;
}

export function loadPanelLayout(storageKey: string = STORAGE_KEY_DEFAULT): PanelLayout {
  try {
    if (typeof window === "undefined" || !window.localStorage) return { ...DEFAULT_LAYOUT };
    const raw = window.localStorage.getItem(storageKey);
    if (raw == null) {
      // 无持久化时按视口决定右栏初始状态
      if (typeof window.innerWidth === "number") {
        return { ...DEFAULT_LAYOUT, rightCollapsed: defaultRightCollapsed(window.innerWidth) };
      }
      return { ...DEFAULT_LAYOUT };
    }
    const parsed: unknown = JSON.parse(raw);
    if (parsed == null || typeof parsed !== "object" || Array.isArray(parsed)) return { ...DEFAULT_LAYOUT };
    const obj = parsed as Record<string, unknown>;
    const out: PanelLayout = { ...DEFAULT_LAYOUT };

    if (typeof obj.leftWidth === "number" && Number.isFinite(obj.leftWidth)) {
      out.leftWidth = clampPanelWidth("left", obj.leftWidth);
    }
    if (typeof obj.rightWidth === "number" && Number.isFinite(obj.rightWidth)) {
      out.rightWidth = clampPanelWidth("right", obj.rightWidth);
    }
    // collapsed 字段存在时才尊重，否则保留默认值（App 层会按视口覆盖）
    if (typeof obj.leftCollapsed === "boolean") {
      out.leftCollapsed = obj.leftCollapsed;
    }
    if (typeof obj.rightCollapsed === "boolean") {
      out.rightCollapsed = obj.rightCollapsed;
    }
    return out;
  } catch {
    // 坏数据降级为默认
    return { ...DEFAULT_LAYOUT };
  }
}

export function savePanelLayout(layout: PanelLayout, storageKey: string = STORAGE_KEY_DEFAULT): void {
  try {
    if (typeof window === "undefined" || !window.localStorage) return;
    const payload: PanelLayout = {
      leftWidth: clampPanelWidth("left", layout.leftWidth),
      rightWidth: clampPanelWidth("right", layout.rightWidth),
      leftCollapsed: Boolean(layout.leftCollapsed),
      rightCollapsed: Boolean(layout.rightCollapsed),
    };
    window.localStorage.setItem(storageKey, JSON.stringify(payload));
  } catch {
    // 存储失败静默忽略（无痕/配额等）
  }
}
