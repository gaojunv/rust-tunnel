/**
 * @vitest-environment jsdom
 *
 * hover-preview 测试：
 * - `stripFrontmatter` / `firstParagraph` / `stripMarkdown` / `extractSummary` 纯函数
 * - `PreviewCache` LRU 语义（命中 refresh、超容淘汰最旧）
 * - hover 流程：延迟弹出、缓存命中不打后端、不存在态、destroy 清理（timer 泄漏）
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import type { NoteDto } from "@/api/types";

vi.mock("@/api/tauri", () => ({
  getNote: vi.fn(),
}));

import { getNote } from "@/api/tauri";
import {
  extractSummary,
  stripFrontmatter,
  firstParagraph,
  stripMarkdown,
  PreviewCache,
  attachWikilinkHover,
  HOVER_DELAY_MS,
  PREVIEW_CACHE_CAPACITY,
  PREVIEW_SUMMARY_MAX_LEN,
  __testIsPopoverOpen,
  __testResetHover,
} from "./hover-preview";
import { wikilinkNavFacet } from "./widgets";

const getNoteMock = vi.mocked(getNote);

function note(key: string, body: string, title?: string): NoteDto {
  return { key, title: title ?? key, aliases: [], tags: [], body, modified: 0 };
}

function makeView(): EditorView {
  const parent = document.createElement("div");
  document.body.appendChild(parent);
  const view = new EditorView({
    parent,
    state: EditorState.create({
      doc: "",
      extensions: [wikilinkNavFacet.of(() => {})],
    }),
  });
  return view;
}

function destroyView(view: EditorView): void {
  view.dom.parentElement?.remove();
  view.destroy();
}

beforeEach(() => {
  vi.useFakeTimers();
  getNoteMock.mockReset();
  __testResetHover();
});

afterEach(() => {
  __testResetHover();
  document.querySelectorAll(".cm-lp-hover-popover").forEach((el) => el.remove());
  vi.useRealTimers();
});

// 纯微任务推进（fake timers 下不能用真实 setTimeout，会死锁）。
// 动态 import 模块链较深，需要较多轮次。
async function flush(rounds = 100): Promise<void> {
  for (let i = 0; i < rounds; i++) {
    await Promise.resolve();
  }
}

// ── stripFrontmatter ───────────────────────────────────────────────────────

describe("stripFrontmatter", () => {
  it("去掉开头的 --- … --- 块", () => {
    const out = stripFrontmatter("---\ntitle: hi\ntags: [a]\n---\nbody here");
    expect(out).toBe("body here");
  });

  it("支持 ... 闭合", () => {
    expect(stripFrontmatter("---\ntitle: hi\n...\nbody")).toBe("body");
  });

  it("非 --- 开头原样返回", () => {
    const src = "# title\nbody";
    expect(stripFrontmatter(src)).toBe(src);
  });

  it("未闭合原样返回", () => {
    const src = "---\ntitle: hi\nbody";
    expect(stripFrontmatter(src)).toBe(src);
  });
});

// ── firstParagraph ─────────────────────────────────────────────────────────

describe("firstParagraph", () => {
  it("取首个非空段落", () => {
    expect(firstParagraph("\n\nfirst para\nline2\n\nsecond")).toBe("first para\nline2");
  });

  it("全空返回空串", () => {
    expect(firstParagraph("  \n \n")).toBe("");
  });

  it("纯标题段落跳过，取首个正文段", () => {
    expect(firstParagraph("# 标题\n\n正文段\n\n第二段")).toBe("正文段");
  });

  it("全是标题时回退到首段", () => {
    expect(firstParagraph("# 标题")).toBe("# 标题");
  });
});

// ── stripMarkdown ──────────────────────────────────────────────────────────

describe("stripMarkdown", () => {
  it("标题/引用/列表前缀去掉", () => {
    expect(stripMarkdown("## 标题文字")).toBe("标题文字");
    expect(stripMarkdown("> 引用内容")).toBe("引用内容");
    expect(stripMarkdown("- 列表项")).toBe("列表项");
    expect(stripMarkdown("1. 有序项")).toBe("有序项");
    expect(stripMarkdown("- [ ] 任务项")).toBe("任务项");
  });

  it("链接/图片/wikilink 取文本", () => {
    expect(stripMarkdown("[文本](http://x.com)")).toBe("文本");
    expect(stripMarkdown("![alt](img.png)")).toBe("alt");
    expect(stripMarkdown("[[target|显示名]]")).toBe("显示名");
    expect(stripMarkdown("[[target]]")).toBe("target");
  });

  it("加粗/斜体/删除线/高亮/行内码去符号", () => {
    expect(stripMarkdown("**粗**")).toBe("粗");
    expect(stripMarkdown("*斜*")).toBe("斜");
    expect(stripMarkdown("~~删~~")).toBe("删");
    expect(stripMarkdown("==亮==")).toBe("亮");
    expect(stripMarkdown("`码`")).toBe("码");
  });

  it("围栏标记行跳过", () => {
    expect(stripMarkdown("```js\ncode\n```")).toBe("code");
  });
});

// ── extractSummary ─────────────────────────────────────────────────────────

describe("extractSummary", () => {
  it("frontmatter + 标题 + 首段 → 摘要", () => {
    const body = "---\ntitle: t\n---\n# 标题\n\n这是**首段**内容，含 [[link|链接]]。\n\n第二段。";
    const s = extractSummary(body);
    expect(s).toContain("这是首段内容，含 链接。");
    expect(s).not.toContain("第二段");
  });

  it("超长截断到上限并补 …", () => {
    const s = extractSummary("a".repeat(PREVIEW_SUMMARY_MAX_LEN + 50));
    expect(s.length).toBeLessThanOrEqual(PREVIEW_SUMMARY_MAX_LEN + 1);
    expect(s.endsWith("…")).toBe(true);
  });

  it("空正文返回空串", () => {
    expect(extractSummary("")).toBe("");
    expect(extractSummary("---\na: b\n---\n")).toBe("");
  });
});

// ── PreviewCache ───────────────────────────────────────────────────────────

describe("PreviewCache", () => {
  it("miss 返回 undefined，set 后 hit", () => {
    const c = new PreviewCache(10);
    expect(c.get("a")).toBeUndefined();
    c.set("a", { title: "A", summary: "s", notFound: false });
    expect(c.get("a")).toEqual({ title: "A", summary: "s", notFound: false });
  });

  it("超容淘汰最旧（未访问的）", () => {
    const c = new PreviewCache(2);
    c.set("a", { title: "A", summary: "", notFound: false });
    c.set("b", { title: "B", summary: "", notFound: false });
    c.set("c", { title: "C", summary: "", notFound: false });
    expect(c.get("a")).toBeUndefined();
    expect(c.get("b")).toBeDefined();
    expect(c.get("c")).toBeDefined();
  });

  it("get refresh：刚访问的不被淘汰", () => {
    const c = new PreviewCache(2);
    c.set("a", { title: "A", summary: "", notFound: false });
    c.set("b", { title: "B", summary: "", notFound: false });
    c.get("a");
    c.set("c", { title: "C", summary: "", notFound: false });
    expect(c.get("a")).toBeDefined();
    expect(c.get("b")).toBeUndefined();
  });

  it("默认容量 100", () => {
    expect(PREVIEW_CACHE_CAPACITY).toBe(100);
    const c = new PreviewCache();
    for (let i = 0; i < 120; i++) c.set(`k${i}`, { title: `${i}`, summary: "", notFound: false });
    expect(c.size).toBe(100);
    expect(c.get("k0")).toBeUndefined();
    expect(c.get("k119")).toBeDefined();
  });
});

// ── hover 流程 ─────────────────────────────────────────────────────────────

describe("attachWikilinkHover", () => {
  it("悬停不足延迟不弹出；满延迟弹出 loading → 命中后显示标题摘要", async () => {
    const view = makeView();
    try {
      let resolveNote!: (n: NoteDto) => void;
      getNoteMock.mockReturnValue(new Promise<NoteDto>((r) => { resolveNote = r; }));
      const el = document.createElement("span");
      view.dom.appendChild(el);
      const cleanup = attachWikilinkHover(el, view, "目标", () => {});
      try {
        el.dispatchEvent(new MouseEvent("mouseenter", { bubbles: true }));
        vi.advanceTimersByTime(HOVER_DELAY_MS - 50);
        await flush();
        expect(__testIsPopoverOpen()).toBe(false);

        vi.advanceTimersByTime(50);
        await flush();
        expect(__testIsPopoverOpen()).toBe(true);
        let pop = document.querySelector(".cm-lp-hover-popover");
        expect(pop?.textContent).toContain("加载中");
        expect(getNoteMock).toHaveBeenCalledTimes(1);

        resolveNote(note("目标", "# T\n\n摘要内容在这里。"));
        await flush();
        pop = document.querySelector(".cm-lp-hover-popover");
        expect(pop?.textContent).toContain("目标");
        expect(pop?.textContent).toContain("摘要内容在这里。");
      } finally {
        cleanup();
      }
    } finally {
      destroyView(view);
    }
  });

  it("缓存命中不打后端", async () => {
    const view = makeView();
    try {
      getNoteMock.mockResolvedValue(note("缓存键", "正文"));
      // 第一次：打后端
      const el1 = document.createElement("span");
      view.dom.appendChild(el1);
      const c1 = attachWikilinkHover(el1, view, "缓存键", () => {});
      el1.dispatchEvent(new MouseEvent("mouseenter", { bubbles: true }));
      vi.advanceTimersByTime(HOVER_DELAY_MS);
      await flush();
      await flush();
      expect(getNoteMock).toHaveBeenCalledTimes(1);
      c1();
      el1.remove();

      // 第二次：命中缓存
      const el2 = document.createElement("span");
      view.dom.appendChild(el2);
      const c2 = attachWikilinkHover(el2, view, "缓存键", () => {});
      try {
        el2.dispatchEvent(new MouseEvent("mouseenter", { bubbles: true }));
        vi.advanceTimersByTime(HOVER_DELAY_MS);
        await flush();
        await flush();
        expect(getNoteMock).toHaveBeenCalledTimes(1);
        expect(document.querySelector(".cm-lp-hover-popover")?.textContent).toContain("缓存键");
      } finally {
        c2();
      }
    } finally {
      destroyView(view);
    }
  });

  it("笔记不存在 → 「笔记不存在（点击创建）」+ 打开按钮走导航回调", async () => {
    const view = makeView();
    try {
      getNoteMock.mockRejectedValue(new Error("笔记不存在: 幽灵"));
      const el = document.createElement("span");
      view.dom.appendChild(el);
      const onOpen = vi.fn();
      const cleanup = attachWikilinkHover(el, view, "幽灵", onOpen);
      try {
        el.dispatchEvent(new MouseEvent("mouseenter", { bubbles: true }));
        vi.advanceTimersByTime(HOVER_DELAY_MS);
        await flush();
        await flush();
        const pop = document.querySelector(".cm-lp-hover-popover");
        expect(pop?.textContent).toContain("笔记不存在（点击创建）");
        const btn = pop?.querySelector(".cm-lp-hover-open") as HTMLButtonElement | null;
        expect(btn?.textContent).toBe("打开");
        btn?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        expect(onOpen).toHaveBeenCalledTimes(1);
      } finally {
        cleanup();
      }
    } finally {
      destroyView(view);
    }
  });

  it("mouseleave 取消 timer：不再弹出（无泄漏）", async () => {
    const view = makeView();
    try {
      getNoteMock.mockResolvedValue(note("x", "y"));
      const el = document.createElement("span");
      view.dom.appendChild(el);
      const cleanup = attachWikilinkHover(el, view, "x", () => {});
      try {
        el.dispatchEvent(new MouseEvent("mouseenter", { bubbles: true }));
        el.dispatchEvent(new MouseEvent("mouseleave", { bubbles: true }));
        vi.advanceTimersByTime(HOVER_DELAY_MS + 1000);
        await flush();
        expect(__testIsPopoverOpen()).toBe(false);
        expect(getNoteMock).not.toHaveBeenCalled();
      } finally {
        cleanup();
      }
    } finally {
      destroyView(view);
    }
  });

  it("cleanup 后 timer 不触发（widget 销毁语义）", async () => {
    const view = makeView();
    try {
      getNoteMock.mockResolvedValue(note("y", "z"));
      const el = document.createElement("span");
      view.dom.appendChild(el);
      const cleanup = attachWikilinkHover(el, view, "y", () => {});
      el.dispatchEvent(new MouseEvent("mouseenter", { bubbles: true }));
      cleanup();
      el.remove();
      vi.advanceTimersByTime(HOVER_DELAY_MS + 1000);
      await flush();
      expect(__testIsPopoverOpen()).toBe(false);
      expect(getNoteMock).not.toHaveBeenCalled();
    } finally {
      destroyView(view);
    }
  });

  it("切换悬停目标：旧请求回来不覆盖新 popover", async () => {
    const view = makeView();
    try {
      let resolveA!: (n: NoteDto) => void;
      getNoteMock.mockImplementation((key: string) => {
        if (key === "A") return new Promise<NoteDto>((r) => { resolveA = r; });
        return Promise.resolve(note("B", "B 的摘要"));
      });
      const elA = document.createElement("span");
      const elB = document.createElement("span");
      view.dom.append(elA, elB);
      const cA = attachWikilinkHover(elA, view, "A", () => {});
      const cB = attachWikilinkHover(elB, view, "B", () => {});
      try {
        elA.dispatchEvent(new MouseEvent("mouseenter", { bubbles: true }));
        vi.advanceTimersByTime(HOVER_DELAY_MS);
        await flush();
        // 切到 B（A 的请求仍 pending）
        elA.dispatchEvent(new MouseEvent("mouseleave", { bubbles: true }));
        elB.dispatchEvent(new MouseEvent("mouseenter", { bubbles: true }));
        vi.advanceTimersByTime(HOVER_DELAY_MS);
        await flush();
        await flush();
        // A 的慢请求此时回来，不能覆盖 B 的内容
        resolveA(note("A", "A 的摘要"));
        await flush();
        await flush();
        expect(document.querySelector(".cm-lp-hover-popover")?.textContent).toContain("B 的摘要");
      } finally {
        cA();
        cB();
      }
    } finally {
      destroyView(view);
    }
  });
});
