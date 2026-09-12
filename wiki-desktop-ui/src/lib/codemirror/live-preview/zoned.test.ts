/**
 * zoned 全集 + 按选区过滤 与具体计算的等价性测试。
 *
 * 对一批文档 × 一批选区断言：
 *   filterZonedSpecs(computeZonedSpecs(state), sel) === computeLivePreviewSpecs(state, 0, len)
 * （`toEqual` 同时校验内容与顺序）。
 *
 * 选区覆盖：文档各行行首/行中/行尾（含构造边界、pad 外扩边界），
 * 外加多选区与区间选区组合。
 */
import { describe, it, expect } from "vitest";
import { EditorState, EditorSelection } from "@codemirror/state";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import {
  computeLivePreviewSpecs,
  computeZonedSpecs,
  filterZonedSpecs,
} from "./spec";

function makeState(doc: string, sel: { anchor: number; head?: number }[]): EditorState {
  return EditorState.create({
    doc,
    selection: EditorSelection.create(sel.map((s) => EditorSelection.range(s.anchor, s.head ?? s.anchor))),
    extensions: [markdown({ base: markdownLanguage }), EditorState.allowMultipleSelections.of(true)],
  });
}

/** 选区采样：各行行首/行中/行尾 + 文档首尾 + 跨行区间 + 多选区。 */
function sampleSelections(doc: string): { anchor: number; head?: number }[][] {
  const lines = doc.split("\n");
  const out: { anchor: number; head?: number }[][] = [[{ anchor: 0 }]];
  let pos = 0;
  for (const ln of lines) {
    const from = pos;
    const to = pos + ln.length;
    out.push([{ anchor: from }]);
    out.push([{ anchor: to }]);
    if (ln.length > 2) out.push([{ anchor: from + 1 }], [{ anchor: to - 1 }]);
    if (ln.length > 4) out.push([{ anchor: from + 2 }]);
    pos = to + 1;
  }
  out.push([{ anchor: doc.length }]);
  // 跨行区间选区
  if (doc.length > 10) {
    out.push([{ anchor: 0, head: Math.min(10, doc.length) }]);
    out.push([{ anchor: Math.max(0, doc.length - 10), head: doc.length }]);
  }
  // 多选区：首 + 尾
  out.push([{ anchor: 0 }, { anchor: doc.length }]);
  if (doc.length > 5) out.push([{ anchor: 2 }, { anchor: doc.length - 2 }]);
  return out;
}

const DOCS: [string, string][] = [
  ["plain", "hello world\nsecond line\n"],
  ["atx", "# Hello\nbody **bold** tail\n"],
  ["atx-closed", "# Title #\n## H2 ##\n"],
  ["atx-empty", "#\nbody\n"],
  ["setext", "My Title\n========\nbody\n"],
  ["setext2", "Sub\n---\nbody\n"],
  ["setext-bold", "Title **bold**\n===\n\ntail\n"],
  ["setext-autolink", "see <http://x.com>\n===\n\ntail\n"],
  ["quote", "> a\n> b\nbody\n"],
  ["callout", "> [!note] Title\n> content\nbody\n"],
  ["callout-notfirst", "> line1\n> [!note] here\nbody\n"],
  ["hr", "text\n\n---\n\ntail\n"],
  ["hr-only", "---\n"],
  ["tasks", "- [ ] todo\n- [x] done\n- [X] caps\n"],
  ["bullets", "- a\n+ b\n* c\n1. d\n"],
  ["fence", "```js\ncode here\n```\ntail\n"],
  ["fence-open-cursor", "```js\ncode here\n```\n"],
  ["fence-unclosed", "```js\ncode1\ncode2\n"],
  ["fence-wikilink", "```\n[[not-a-link]]\n```\ntail\n"],
  ["emph-multiline", "*foo\nbar* tail\n"],
  ["link-multiline", "[text\nmore](http://x.com)\n"],
  ["image-block", "![photo](assets/one.jpg)\n\nsome text\n"],
  ["image-inline", "text ![pic](http://a.com/i.png) more\n"],
  ["image-size", "![a|100x50](x.png)\n\ntail\n"],
  ["image-angle", "![a](<http://x.com/a b.png>)\n\n"],
  ["image-heading", "# T ![a](http://x.com)\n"],
  ["link", "A [click here](https://example.com) B\n"],
  ["link-ref", "A [text][ref] B\n\n[ref]: http://x.com\n"],
  ["link-ref-collapsed", "A [guide][] B\n\n[guide]: http://g.com\n"],
  ["link-ref-missing", "A [text][ref] B\n"],
  ["autolink", "see <http://x.com> here\n"],
  ["table", "| a | b |\n|---|---|\n| 1 | 2 |\n"],
  ["table-align", "| l | c | r |\n|:---|:---:|---:|\n| x | y | z |\n"],
  ["table-wikilink", "| [[w]] |\n|---|\n| x |\n"],
  ["table-image", "| ![a](u) |\n|---|\n| x |\n"],
  ["table-link", "| [t](http://x.com) |\n|---|\n| z |\n"],
  ["table-ref", "| [text][r] |\n|---|\n| z |\n\n[r]: http://x.com\n"],
  ["table-hl", "| ==hl== |\n|---|\n| z |\n"],
  ["table-math", "| $x$ |\n|---|---|\n| z |\n"],
  ["table-autolink", "| <http://a.com> |\n|---|\n| z |\n"],
  ["table-strong", "| a | b |\n|---|---|\n| **x** | [[w]] |\n"],
  ["table-embed", "| ![[x.png]] |\n|---|\n| z |\n"],
  ["table-quote", "> | a |\n> |---|\n> | x |\n"],
  ["wikilink", "go [[target|label]] and [[bare]]\n\ntail\n"],
  ["wikilink-hash", "[[a#b#c]]\n\ntail\n"],
  ["embed-img", "![[assets/a.png]]\n\nsome text\n"],
  ["embed-size", "![[b.jpg|300]]\n\ntail\n"],
  ["embed-note", "![[某笔记]]\n\ntail\n"],
  ["embed-inline", "文字 ![[x.png]] 文字\n"],
  ["fm", "---\ntitle: hi\nauthor: me\n---\nbody\n"],
  ["fm-dots", "---\ntitle: hi\n...\nbody\n"],
  ["fm-single", "---\n---\nbody\n"],
  ["fm-noclose", "---\ntitle: hi\nbody\n"],
  ["fm-notfirst", "text\n---\ntitle: hi\n---\n"],
  ["fm-wikilink", "---\ntitle: hi\nref: [[note]]\n---\nbody\n"],
  ["fm-hl", "---\ntitle: ==no==\n---\nbody\n"],
  ["highlight", "==hl==\n\ntail\n"],
  ["highlight-multi", "==a== and ==b==\n\ntail\n"],
  ["highlight-code", "`==not hl==`\n\ntail\n"],
  ["highlight-triple", "===not hl===\n\ntail\n"],
  ["highlight-heading", "# Title ==hl==\n\ntail\n"],
  ["heading-inline", "# Title **bold** [link](http://x.com)\n\ntail\n"],
  ["heading-code", "# Title `code`\n\ntail\n"],
  ["heading-wikilink", "# [[target|label]]\n\ntail\n"],
  ["math-single", "$$x^2$$\n\ntail\n"],
  ["math-multi", "$$\na+b\nc\n$$\n\ntail\n"],
  ["math-inline", "有 $e=mc^2$ 个定理\n"],
  ["math-money", "价格 $5 和 $10 共 $15\n"],
  ["math-code", "用 `$x$` 表示\n"],
  ["math-fence", "```\n$\na\n$\n```\n"],
  ["math-unclosed", "$$\na\nb\n"],
  ["math-spaces", "$$  x^2  $$\n\ntail\n"],
  ["math-multiline-inner", "$$\na$b\nc$d\ne\n$$\n\ntail\n"],
  ["math-table", "| a |\n|---|\n$$x$$\n"],
  ["combo", "---\ntitle: t\n---\n\n# H **b** [[w]]\n\n> [!tip] Go\n> body\n\n```py\nprint(1)\n```\n\n| a | b |\n|---|---|\n| ==h== | $x$ |\n\n$$y$$\n\n- [ ] task\n\ntail [[end]]\n"],
  ["index-doc", "---\nref: abc\n---\n\ntext\n\n---\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n$$\nx^2\n$$\n\ntail\n"],
];

describe("zoned equivalence", () => {
  for (const [name, doc] of DOCS) {
    it(`doc ${name}: zoned+filter === concrete for all sampled selections`, () => {
      // zoned 全集只与文档内容有关：任取一选区构建后复用于全部选区
      const anyState = makeState(doc, [{ anchor: 0 }]);
      const zoned = computeZonedSpecs(anyState);
      for (const sel of sampleSelections(doc)) {
        const state = makeState(doc, sel);
        const expected = computeLivePreviewSpecs(state, 0, state.doc.length);
        const actual = filterZonedSpecs(zoned, state.selection.ranges);
        expect(actual, `doc=${name} sel=${JSON.stringify(sel)}`).toEqual(expected);
      }
    });
  }

  it("zoned cache reusable across selection states (same doc object)", () => {
    const doc = "| a |\n|---|\n| **x** [[w]] |\n";
    const s1 = makeState(doc, [{ anchor: 0 }]);
    const zoned = computeZonedSpecs(s1);
    const positions = [0, 2, 5, 10, 15, 20, doc.length];
    for (const p of positions) {
      const st = makeState(doc, [{ anchor: p }]);
      expect(filterZonedSpecs(zoned, st.selection.ranges)).toEqual(
        computeLivePreviewSpecs(st, 0, st.doc.length),
      );
    }
  });
});
