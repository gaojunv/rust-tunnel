/**
 * @vitest-environment jsdom
 *
 * Live Preview 视图适配层冒烟测试（jsdom）：
 * - 挂载不抛错、标题 HeaderMark 被隐藏
 * - checkbox 点击翻转原文 `[ ]` → `[x]`
 * - Compartment.reconfigure 卸载后装饰清空（撤销历史保留由 CM 保证）
 */
import { describe, it, expect, afterEach } from "vitest";
import { Compartment, EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { history } from "@codemirror/commands";
import { wikiSyntaxHighlighting } from "../theme";
import { livePreview, livePreviewPlugin, wikilinkNavFacet } from "./index";

let view: EditorView | null = null;

afterEach(() => {
  view?.destroy();
  view = null;
  document.body.innerHTML = "";
});

function mount(doc: string, enabled = true): EditorView {
  const compartment = new Compartment();
  view = new EditorView({
    state: EditorState.create({
      doc,
      extensions: [
        history(),
        EditorState.allowMultipleSelections.of(true),
        markdown({ base: markdownLanguage }),
        wikiSyntaxHighlighting,
        wikilinkNavFacet.of(() => {}),
        compartment.of(enabled ? livePreview() : []),
      ],
    }),
    parent: document.body,
  });
  return view;
}

describe("livePreview view adapter", () => {
  it("挂载后标题 HeaderMark 被隐藏", () => {
    const v = mount("# Hello\n");
    // 光标默认在 0（标题行内）→ 标题行还原；把光标移到末尾再断言
    v.dispatch({ selection: { anchor: v.state.doc.length } });
    const plugin = v.plugin(livePreviewPlugin);
    expect(plugin).not.toBeNull();
    const set = plugin!.decorations;
    // 至少包含开标记符的 hide 装饰（replace 空）
    let count = 0;
    const it = set.iter();
    while (it.value) {
      count++;
      it.next();
    }
    expect(count).toBeGreaterThan(0);
    // DOM 层面：原文 "#" 不可见（被 replace 为空）
    expect(v.dom.textContent ?? "").not.toContain("#");
  });

  it("checkbox 点击翻转原文", () => {
    const v = mount("- [ ] todo\n");
    v.dispatch({ selection: { anchor: v.state.doc.length } });
    const box = v.dom.querySelector(".cm-lp-checkbox-input") as HTMLInputElement | null;
    expect(box).not.toBeNull();
    expect(box!.checked).toBe(false);
    box!.click();
    expect(v.state.doc.toString()).toBe("- [x] todo\n");
  });

  it("reconfigure 卸载后只剩原文（装饰清空）", () => {
    const compartment = new Compartment();
    view = new EditorView({
      state: EditorState.create({
        doc: "# Hello\n",
        extensions: [
          markdown({ base: markdownLanguage }),
          compartment.of(livePreview()),
        ],
      }),
      parent: document.body,
    });
    view.dispatch({ selection: { anchor: view.state.doc.length } });
    let plugin = view.plugin(livePreviewPlugin);
    expect(plugin!.decorations.size).toBeGreaterThan(0);
    view.dispatch({ effects: compartment.reconfigure([]) });
    plugin = view.plugin(livePreviewPlugin);
    // 插件已卸载
    expect(plugin).toBeNull();
    expect(view.dom.textContent ?? "").toContain("# Hello");
  });

  it("wikilink 点击触发导航回调", () => {
    const seen: string[] = [];
    view = new EditorView({
      state: EditorState.create({
        doc: "go [[target|label]]\n\ntail\n",
        extensions: [
          markdown({ base: markdownLanguage }),
          wikilinkNavFacet.of((key: string) => {
            seen.push(key);
          }),
          livePreview(),
        ],
      }),
      parent: document.body,
    });
    view.dispatch({ selection: { anchor: view.state.doc.length } });
    const link = view.dom.querySelector(".cm-lp-wikilink") as HTMLElement | null;
    expect(link).not.toBeNull();
    expect(link!.textContent).toBe("label");
    link!.click();
    expect(seen).toEqual(["target"]);
  });
});
