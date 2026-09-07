/**
 * 编辑器偏好（localStorage 持久化），容错读写仿 `panel-layout.ts`。
 * key: `wiki.editor.prefs.v1` → `{ livePreview: boolean }`（默认 true）。
 */

export interface EditorPrefs {
  livePreview: boolean;
}

const STORAGE_KEY = "wiki.editor.prefs.v1";

const DEFAULT_PREFS: EditorPrefs = {
  livePreview: true,
};

export function loadEditorPrefs(storageKey: string = STORAGE_KEY): EditorPrefs {
  try {
    if (typeof window === "undefined" || !window.localStorage) return { ...DEFAULT_PREFS };
    const raw = window.localStorage.getItem(storageKey);
    if (raw == null) return { ...DEFAULT_PREFS };
    const parsed: unknown = JSON.parse(raw);
    if (parsed == null || typeof parsed !== "object" || Array.isArray(parsed)) {
      return { ...DEFAULT_PREFS };
    }
    const obj = parsed as Record<string, unknown>;
    const out: EditorPrefs = { ...DEFAULT_PREFS };
    if (typeof obj.livePreview === "boolean") {
      out.livePreview = obj.livePreview;
    }
    return out;
  } catch {
    // 坏数据降级为默认
    return { ...DEFAULT_PREFS };
  }
}

export function saveEditorPrefs(prefs: EditorPrefs, storageKey: string = STORAGE_KEY): void {
  try {
    if (typeof window === "undefined" || !window.localStorage) return;
    const payload: EditorPrefs = {
      livePreview: Boolean(prefs.livePreview),
    };
    window.localStorage.setItem(storageKey, JSON.stringify(payload));
  } catch {
    // 存储失败静默忽略（无痕/配额等）
  }
}
