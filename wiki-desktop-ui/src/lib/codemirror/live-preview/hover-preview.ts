import type { EditorView } from "@codemirror/view";
import type { NoteDto } from "@/api/types";

/**
 * wikilink 悬停预览。
 *
 * - 悬停 300ms 后弹出 popover：标题 + 摘要（首个段落，截断 ~250 字）
 * - 加载中 → loading 态；getNote 抛错 →「笔记不存在（点击创建）」态
 * - getNote 结果缓存（Map + 容量上限 100），避免反复悬停反复打后端
 * - popover 挂载到 `document.body`（fixed 定位），滚动/resize/点击别处时关闭
 * - widget 销毁时经 `detachWikilinkHover` 清理监听 + timer（`WikilinkWidget.destroy`）
 */

// ── 摘要提取 ────────────────────────────────────────────────────────────────

/** 最大摘要长度（字符） */
export const PREVIEW_SUMMARY_MAX_LEN = 250;

/**
 * 从笔记正文提取摘要：去 frontmatter → 取首个非空段落 → 去 markdown 符号 →
 * 截断到 PREVIEW_SUMMARY_MAX_LEN（截断处补 …）。
 */
export function extractSummary(body: string): string {
  let text = stripFrontmatter(body);
  const para = firstParagraph(text);
  if (!para) return "";
  const plain = stripMarkdown(para);
  if (!plain) return "";
  if (plain.length <= PREVIEW_SUMMARY_MAX_LEN) return plain;
  return plain.slice(0, PREVIEW_SUMMARY_MAX_LEN).trimEnd() + "…";
}

/** 去掉文档开头的 YAML frontmatter（`---` … `---` / `...`）。 */
export function stripFrontmatter(body: string): string {
  const lines = body.split("\n");
  if (lines[0]?.trim() !== "---") return body;
  for (let i = 1; i < lines.length; i++) {
    const t = lines[i]?.trim();
    if (t === "---" || t === "...") {
      return lines.slice(i + 1).join("\n");
    }
  }
  return body;
}

/**
 * 取首个非空段落（空行分隔）。
 * 跳过纯标题段落——标题通常与 popover 标题重复；全是标题时回退到首段。
 */
export function firstParagraph(text: string): string {
  const paras = text
    .split(/\n\s*\n/)
    .map((p) => p.trim())
    .filter((p) => p.length > 0);
  if (paras.length === 0) return "";
  return paras.find((p) => !isHeadingOnly(p)) ?? paras[0] ?? "";
}

/** 段落是否仅由 ATX 标题行组成。 */
function isHeadingOnly(para: string): boolean {
  const lines = para
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0);
  return lines.length > 0 && lines.every((l) => /^#{1,6}\s+\S/.test(l));
}

/**
 * 轻量去 markdown 符号（无新依赖，纯正则）：
 * 围栏代码块→取内容首行、行内码去反引号、图片→alt、链接/wikilink→文本、
 * 标题/引用/列表前缀、加粗/斜体/删除线/高亮符号。
 */
export function stripMarkdown(md: string): string {
  const out: string[] = [];
  for (const rawLine of md.split("\n")) {
    let line = rawLine;
    // 围栏标记行跳过
    if (/^\s*(`{3,}|~{3,})/.test(line)) continue;
    // 标题前缀
    line = line.replace(/^\s{0,3}#{1,6}\s+/, "");
    // 引用前缀
    line = line.replace(/^\s{0,3}>\s?/, "");
    // 列表前缀（-/+/数字./task checkbox）
    line = line.replace(/^\s*([-*+]|\d+[.)])\s+(\[[ xX]\]\s+)?/, "");
    // 图片 → alt；链接 → 文本；wikilink → label/target
    line = line.replace(/!\[([^\]]*)\]\([^)]*\)/g, "$1");
    line = line.replace(/\[([^\]]*)\]\([^)]*\)/g, "$1");
    line = line.replace(/\[\[([^\]|]+)(\|([^\]]*))?\]\]/g, (_m, target: string, _bar?: string, label?: string) => {
      const l = label?.trim();
      return l || target.trim();
    });
    // 行内码去反引号
    line = line.replace(/`([^`]*)`/g, "$1");
    // 加粗/斜体/删除线/高亮符号
    line = line.replace(/(\*\*|__)(.*?)\1/g, "$2");
    line = line.replace(/(^|\W)\*([^*\n]+)\*(?=\W|$)/g, "$1$2");
    line = line.replace(/(^|\W)_([^_\n]+)_(?=\W|$)/g, "$1$2");
    line = line.replace(/~~(.*?)~~/g, "$1");
    line = line.replace(/==([^=]+)==/g, "$1");
    const t = line.trim();
    if (t) out.push(t);
  }
  return out.join(" ");
}

// ── 预览缓存 ────────────────────────────────────────────────────────────────

/** 缓存上限 */
export const PREVIEW_CACHE_CAPACITY = 100;

/** 单条缓存：标题 + 摘要（notFound 时两者为空并置标记）。 */
export type PreviewCacheEntry = {
  title: string;
  summary: string;
  notFound: boolean;
};

/**
 * 简单 LRU 缓存（Map 插入序 + get 时 refresh）。
 * key 为笔记 target（大小写敏感，与 getNote 一致）。
 */
export class PreviewCache {
  private map = new Map<string, PreviewCacheEntry>();

  constructor(private readonly capacity: number = PREVIEW_CACHE_CAPACITY) {}

  get(key: string): PreviewCacheEntry | undefined {
    const v = this.map.get(key);
    if (v === undefined) return undefined;
    // refresh：删了重插，保持最近使用在尾部
    this.map.delete(key);
    this.map.set(key, v);
    return v;
  }

  set(key: string, entry: PreviewCacheEntry): void {
    if (this.map.has(key)) this.map.delete(key);
    this.map.set(key, entry);
    while (this.map.size > this.capacity) {
      const oldest = this.map.keys().next();
      if (oldest.done) break;
      this.map.delete(oldest.value);
    }
  }

  clear(): void {
    this.map.clear();
  }

  get size(): number {
    return this.map.size;
  }
}

/** 模块级共享缓存：同编辑器会话内复用。 */
export const previewCache = new PreviewCache();

// ── hover → popover 管理 ────────────────────────────────────────────────────

/** 悬停延迟（ms） */
export const HOVER_DELAY_MS = 300;

/** popover 与锚点/视口的间距（px） */
const POPOVER_GAP = 6;

/** wikilink DOM 上的 hover 状态（挂在元素上，destroy 时清理）。 */
type HoverState = {
  cleanup: (() => void) | null;
};
type HoverDom = HTMLElement & { __wlHover?: HoverState };

/** 当前打开的 popover（同一时间最多一个）。 */
let activePopover: {
  el: HTMLDivElement;
  anchor: HTMLElement;
  onClose: () => void;
} | null = null;

/** 请求序号：防止慢请求覆盖后来的 hover 结果。 */
let requestSeq = 0;

function closeActivePopover(): void {
  if (!activePopover) return;
  const p = activePopover;
  activePopover = null;
  p.onClose();
  p.el.remove();
}

function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/** loading 骨架。 */
function renderLoading(el: HTMLDivElement): void {
  el.innerHTML =
    `<div class="cm-lp-hover-title cm-lp-hover-loading-bar"></div>` +
    `<div class="cm-lp-hover-summary"><span class="cm-lp-hover-loading">加载中…</span></div>`;
}

/** 命中：标题 + 摘要。 */
function renderHit(el: HTMLDivElement, title: string, summary: string): void {
  const titleHtml = `<div class="cm-lp-hover-title">${esc(title)}</div>`;
  const summaryHtml = summary
    ? `<div class="cm-lp-hover-summary">${esc(summary)}</div>`
    : `<div class="cm-lp-hover-summary cm-lp-hover-empty">（空笔记）</div>`;
  // textContent 赋值优先于 innerHTML safety：此处已转义，仅做结构拼接
  el.innerHTML = titleHtml + summaryHtml;
}

/** 未命中：笔记不存在（点击创建）。点击行为与 NoteEditor 不存在态一致：走导航回调。 */
function renderNotFound(el: HTMLDivElement, target: string, onOpen: () => void): void {
  el.innerHTML = "";
  const title = document.createElement("div");
  title.className = "cm-lp-hover-title";
  title.textContent = target;
  const row = document.createElement("div");
  row.className = "cm-lp-hover-summary cm-lp-hover-missing";
  row.textContent = "笔记不存在（点击创建）";
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "cm-lp-hover-open";
  btn.textContent = "打开";
  btn.addEventListener("mousedown", (e) => e.stopPropagation());
  btn.addEventListener("click", (e) => {
    e.preventDefault();
    e.stopPropagation();
    onOpen();
  });
  el.append(title, row, btn);
}

/**
 * 按锚点 rect 定位 popover（fixed 定位，相对视口）：
 * 默认在锚点下方，空间不足翻到上方；水平方向钳制在视口内。
 */
function positionPopover(el: HTMLDivElement, anchor: HTMLElement): void {
  const rect = anchor.getBoundingClientRect();
  // 先保证可见以量尺寸
  el.style.visibility = "hidden";
  el.style.left = "0px";
  el.style.top = "0px";
  const w = el.offsetWidth;
  const h = el.offsetHeight;
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  let left = Math.min(Math.max(rect.left, POPOVER_GAP), Math.max(vw - w - POPOVER_GAP, POPOVER_GAP));
  let top = rect.bottom + POPOVER_GAP;
  if (top + h > vh - POPOVER_GAP && rect.top - h - POPOVER_GAP >= POPOVER_GAP) {
    top = rect.top - h - POPOVER_GAP;
  }
  el.style.left = `${left}px`;
  el.style.top = `${Math.max(top, POPOVER_GAP)}px`;
  el.style.visibility = "";
}

/**
 * 打开 popover 并异步加载内容。
 * onOpenTarget：notFound 态「打开」按钮回调（走 wikilink 导航 → 不存在则 App 侧创建流程）。
 */
function openPopover(view: EditorView, anchor: HTMLElement, target: string, onOpenTarget: () => void): void {
  closeActivePopover();
  const el = document.createElement("div");
  el.className = "cm-lp-hover-popover";
  // popover 内不抢编辑器焦点/选区，不可选中文本
  el.addEventListener("mousedown", (e) => {
    e.preventDefault();
    e.stopPropagation();
  });
  el.addEventListener("click", (e) => {
    e.stopPropagation();
  });
  document.body.appendChild(el);

  const onScrollOrResize = (): void => closeActivePopover();
  const onPointerDown = (e: PointerEvent): void => {
    if (!el.contains(e.target as Node)) closeActivePopover();
  };
  const scroller = view.scrollDOM;
  scroller.addEventListener("scroll", onScrollOrResize, { passive: true });
  window.addEventListener("resize", onScrollOrResize);
  document.addEventListener("pointerdown", onPointerDown, { capture: true });

  activePopover = {
    el,
    anchor,
    onClose: () => {
      scroller.removeEventListener("scroll", onScrollOrResize);
      window.removeEventListener("resize", onScrollOrResize);
      document.removeEventListener("pointerdown", onPointerDown, { capture: true });
    },
  };

  const cached = previewCache.get(target);
  if (cached) {
    if (cached.notFound) renderNotFound(el, target, onOpenTarget);
    else renderHit(el, cached.title, cached.summary);
    positionPopover(el, anchor);
    return;
  }

  renderLoading(el);
  positionPopover(el, anchor);
  const seq = ++requestSeq;
  // 动态 import：首屏 bundle 不拉取 tauri/mock 链；单测里仍走 vi.mock("@/api/tauri")
  const notePromise: Promise<NoteDto> = import("@/api/tauri").then((m) => m.getNote(target));
  void notePromise.then(
    (note) => {
      if (seq !== requestSeq) return;
      if (!activePopover || activePopover.el !== el) return;
      const entry: PreviewCacheEntry = {
        title: note.title || target,
        summary: extractSummary(note.body ?? ""),
        notFound: false,
      };
      previewCache.set(target, entry);
      renderHit(el, entry.title, entry.summary);
      positionPopover(el, anchor);
    },
    () => {
      // getNote 不存在时 reject（见 api/tauri.ts 注释）→ notFound 态；
      // 文案与 NoteEditor 不存在态（「笔记不存在」+ 创建该笔记）语义一致
      if (seq !== requestSeq) return;
      if (!activePopover || activePopover.el !== el) return;
      previewCache.set(target, { title: "", summary: "", notFound: true });
      renderNotFound(el, target, onOpenTarget);
      positionPopover(el, anchor);
    },
  );
}

/**
 * 给 wikilink 元素接 hover 预览。返回清理函数（widget destroy 时调用）。
 *
 * @param el wikilink 渲染元素
 * @param view 所属编辑器视图（取 scrollDOM 做滚动关闭）
 * @param target wikilink 目标 key
 * @param onOpenTarget 「打开」/不存在态点击回调（导航到目标笔记）
 */
export function attachWikilinkHover(
  el: HTMLElement,
  view: EditorView,
  target: string,
  onOpenTarget: () => void,
): () => void {
  const dom = el as HoverDom;
  // 防御：同一元素重复 attach（正常 toDOM 只调一次）
  if (dom.__wlHover) dom.__wlHover.cleanup?.();

  let disposed = false;
  let timer: ReturnType<typeof setTimeout> | null = null;

  const cancelTimer = (): void => {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  };

  const onEnter = (): void => {
    if (disposed) return;
    cancelTimer();
    timer = setTimeout(() => {
      timer = null;
      if (disposed) return;
      openPopover(view, el, target, onOpenTarget);
    }, HOVER_DELAY_MS);
  };

  const onLeave = (): void => {
    cancelTimer();
    // 鼠标从链接移开即关闭（popover 不可交互，保持精简）
    closeActivePopover();
  };

  el.addEventListener("mouseenter", onEnter);
  el.addEventListener("mouseleave", onLeave);

  const cleanup = (): void => {
    disposed = true;
    cancelTimer();
    el.removeEventListener("mouseenter", onEnter);
    el.removeEventListener("mouseleave", onLeave);
    if (dom.__wlHover) dom.__wlHover.cleanup = null;
    // 只有本元素锚定的 popover 才关，避免关掉别的链接的
    if (activePopover?.anchor === el) closeActivePopover();
  };
  dom.__wlHover = { cleanup };
  return cleanup;
}

/** 供单测：当前是否有打开的 popover。 */
export function __testIsPopoverOpen(): boolean {
  return activePopover !== null;
}

/** 供单测：关闭当前 popover 并重置请求序号。 */
export function __testResetHover(): void {
  closeActivePopover();
  requestSeq = 0;
  previewCache.clear();
}
