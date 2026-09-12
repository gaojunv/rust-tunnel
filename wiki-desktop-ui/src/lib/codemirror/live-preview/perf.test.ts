/**
 * @vitest-environment jsdom
 *
 * Live Preview 性能基准（优化前后对比用）：
 * - 构造 ~5000 行混合 markdown 文档
 * - A: computeLivePreviewSpecs 全量计算耗时（node 逻辑，跑在 jsdom 里无妨）
 * - B: StateField 纯 state.update({selection}) 耗时（无 DOM，只跑 update 逻辑）
 * - C: EditorView.dispatch({selection}) 端到端耗时（含 CM 装饰应用/DOM diff）
 *
 * 只 console.log 输出数字，不做时间断言（避免 flaky）。
 */
import { describe, it, expect } from "vitest";
import { EditorState, EditorSelection } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { computeLivePreviewSpecs } from "./spec";
import { livePreviewField } from "./index";

function buildBlock(seed: number): string[] {
  return [
    `# Title ${seed}`,
    ``,
    `Paragraph with **bold** and *em* and \`code\` and ==hl${seed}== and [[link${seed}|Label ${seed}]] and [click](https://example.com/${seed}) and $x^${seed}$ end.`,
    ``,
    `- task item ${seed}`,
    `- [ ] todo ${seed}`,
    `- [x] done ${seed}`,
    `1. ordered ${seed}`,
    ``,
    `> quote ${seed}`,
    `> [!note] Callout ${seed}`,
    `> quoted body ${seed}`,
    ``,
    `---`,
    ``,
    `\`\`\`js`,
    `const x${seed} = [[not-a-link]]; // ==not-hl== $not-math$`,
    `console.log(x${seed});`,
    `\`\`\``,
    ``,
    `| a${seed} | b${seed} |`,
    `|---|---|`,
    `| [[cell]] | ==hl== |`,
    `| 1 | 2 |`,
    ``,
    `Sub ${seed}`,
    `===`,
    ``,
    `$$`,
    `a${seed}+b`,
    `$$`,
    ``,
    `![[img${seed}.png|300]]`,
    ``,
    `![photo](assets/p${seed}.jpg)`,
    ``,
    `text ![pic](http://a.com/i${seed}.png) more`,
    ``,
    `A [text][ref${seed}] B`,
    ``,
    `[ref${seed}]: http://x.com/${seed}`,
    ``,
    `Title **bold${seed}**`,
    ``,
    `My Title ${seed}`,
    `========`,
    ``,
  ];
}

function buildDoc(): string {
  const lines: string[] = [
    `---`,
    `title: bench`,
    `ref: [[fm-link]]`,
    `---`,
    ``,
  ];
  for (let i = 0; i < 100; i++) lines.push(...buildBlock(i));
  // ref 定义（触发 refDefs 全行扫描路径）
  for (let i = 0; i < 100; i++) lines.push(`[extra${i}]: http://y.com/${i}`);
  return lines.join("\n");
}

function makeState(doc: string, anchor: number): EditorState {
  return EditorState.create({
    doc,
    selection: EditorSelection.create([EditorSelection.range(anchor, anchor)]),
    extensions: [markdown({ base: markdownLanguage }), EditorState.allowMultipleSelections.of(true)],
  });
}

// 光标位置序列：散布全文（含构造内部/边界）
function cursorPositions(docLen: number, n: number): number[] {
  const out: number[] = [];
  for (let i = 0; i < n; i++) out.push(Math.floor((docLen * (i + 0.5)) / n));
  return out;
}

function mean(xs: number[]): number {
  return xs.reduce((a, b) => a + b, 0) / xs.length;
}

describe("live-preview perf benchmark", () => {
  it("prints timing numbers (no assertions on time)", () => {
    const doc = buildDoc();
    const lineCount = doc.split("\n").length;
    console.log(`[bench] doc chars=${doc.length} lines=${lineCount}`);

    // ── A: 全量 compute ──
    {
      const st = makeState(doc, doc.length);
      computeLivePreviewSpecs(st, 0, st.doc.length); // warmup
      const ts: number[] = [];
      for (let i = 0; i < 15; i++) {
        const t0 = performance.now();
        computeLivePreviewSpecs(st, 0, st.doc.length);
        ts.push(performance.now() - t0);
      }
      console.log(`[bench] A full computeLivePreviewSpecs: mean=${mean(ts).toFixed(2)}ms over ${ts.length} runs`);
    }

    // ── B: StateField selection-only update（无 DOM）──
    {
      const positions = cursorPositions(doc.length, 60);
      let st = EditorState.create({
        doc,
        extensions: [markdown({ base: markdownLanguage }), livePreviewField],
      });
      st = st.update({ selection: { anchor: doc.length } }).state; // warmup build
      const ts: number[] = [];
      for (const pos of positions) {
        const t0 = performance.now();
        st = st.update({ selection: { anchor: pos } }).state;
        ts.push(performance.now() - t0);
      }
      // 再来一轮（缓存/JIT 稳定后）
      const ts2: number[] = [];
      for (const pos of positions) {
        const t0 = performance.now();
        st = st.update({ selection: { anchor: pos } }).state;
        ts2.push(performance.now() - t0);
      }
      console.log(`[bench] B StateField selection update: mean=${mean(ts).toFixed(3)}ms (cold), mean=${mean(ts2).toFixed(3)}ms (warm) over ${positions.length} moves`);
    }

    // ── C: EditorView.dispatch selection 端到端 ──
    {
      const positions = cursorPositions(doc.length, 30);
      const view = new EditorView({
        state: EditorState.create({
          doc,
          extensions: [markdown({ base: markdownLanguage }), livePreviewField],
        }),
        parent: document.body,
      });
      try {
        view.dispatch({ selection: { anchor: doc.length } }); // warmup
        const ts: number[] = [];
        for (const pos of positions) {
          const t0 = performance.now();
          view.dispatch({ selection: { anchor: pos } });
          ts.push(performance.now() - t0);
        }
        console.log(`[bench] C EditorView dispatch selection: mean=${mean(ts).toFixed(3)}ms over ${positions.length} moves`);
      } finally {
        view.destroy();
        document.body.innerHTML = "";
      }
    }

    expect(true).toBe(true);
  });
});
