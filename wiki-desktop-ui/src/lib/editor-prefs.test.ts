/**
 * @vitest-environment jsdom
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { loadEditorPrefs, saveEditorPrefs } from "./editor-prefs";

const KEY = "wiki.editor.prefs.test";

describe("editor-prefs localStorage", () => {
  beforeEach(() => {
    window.localStorage.removeItem(KEY);
    window.localStorage.removeItem("wiki.editor.prefs.v1");
  });
  afterEach(() => {
    window.localStorage.removeItem(KEY);
    window.localStorage.removeItem("wiki.editor.prefs.v1");
    vi.restoreAllMocks();
  });

  it("默认 livePreview = true", () => {
    expect(loadEditorPrefs(KEY).livePreview).toBe(true);
  });

  it("坏 JSON 降级为默认", () => {
    window.localStorage.setItem(KEY, "{bad-json");
    expect(loadEditorPrefs(KEY)).toEqual({ livePreview: true });
  });

  it("非对象/数组/null 降级", () => {
    window.localStorage.setItem(KEY, JSON.stringify([1, 2]));
    expect(loadEditorPrefs(KEY).livePreview).toBe(true);
    window.localStorage.setItem(KEY, JSON.stringify(null));
    expect(loadEditorPrefs(KEY).livePreview).toBe(true);
  });

  it("缺字段时保留默认", () => {
    window.localStorage.setItem(KEY, JSON.stringify({ other: 1 }));
    expect(loadEditorPrefs(KEY).livePreview).toBe(true);
  });

  it("错误类型字段被忽略", () => {
    window.localStorage.setItem(KEY, JSON.stringify({ livePreview: "true" }));
    expect(loadEditorPrefs(KEY).livePreview).toBe(true);
    window.localStorage.setItem(KEY, JSON.stringify({ livePreview: 1 }));
    expect(loadEditorPrefs(KEY).livePreview).toBe(true);
  });

  it("布尔字段正常读取", () => {
    window.localStorage.setItem(KEY, JSON.stringify({ livePreview: false }));
    expect(loadEditorPrefs(KEY).livePreview).toBe(false);
  });

  it("save/load 往返", () => {
    saveEditorPrefs({ livePreview: false }, KEY);
    expect(loadEditorPrefs(KEY).livePreview).toBe(false);
    saveEditorPrefs({ livePreview: true }, KEY);
    expect(loadEditorPrefs(KEY).livePreview).toBe(true);
  });

  it("localStorage 异常时 load 降级、save 静默", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(loadEditorPrefs(KEY).livePreview).toBe(true);
    vi.restoreAllMocks();
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("quota");
    });
    expect(() => saveEditorPrefs({ livePreview: false }, KEY)).not.toThrow();
  });
});
