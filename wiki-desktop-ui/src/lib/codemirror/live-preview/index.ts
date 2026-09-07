import type { Extension } from "@codemirror/state";
import {
  Decoration,
  EditorView,
  ViewPlugin,
  ViewUpdate,
  type DecorationSet,
} from "@codemirror/view";
import { computeLivePreviewSpecs } from "./spec";
import { CheckboxWidget, HrWidget, WikilinkWidget } from "./widgets";
import { livePreviewTheme } from "./theme";

export { wikilinkNavFacet } from "./widgets";

/**
 * Live Preview 视图适配层：DecoSpec → Decoration。
 *
 * - 仅对 `view.visibleRanges` 逐段调用纯函数核心；
 * - `docChanged || selectionSet || viewportChanged` 时重算（`syntaxTree` 增量解析）；
 * - 排序：行装饰用 `line()`（按行号递增天然有序）；行内按 `from` 递增建集，
 *   CM6 RangeSetBuilder 对重叠 mark 自动分层。
 *
 * 映射约定：
 * - `line` → `Decoration.line`
 * - `mark` → `Decoration.mark`
 * - `hide` → `Decoration.replace({})`（零宽，不占位）
 * - `checkbox` / `hr` / `wikilink` → `Decoration.replace({ widget })`
 *
 * 隐藏区间与 widget 区间必须避免部分重叠：同一标题的 HeaderMark 分属不同
 * 区间，彼此不交叠；checkbox 与同行 ListMark 区间分离（`[ ]` vs `-`）。
 * 同一起点（如行首开标记与行装饰）由不同装饰通道承载，无冲突。
 */
function buildDecoSet(view: EditorView): DecorationSet {
  const lineDecos: { line: number; deco: Decoration }[] = [];
  const inlineDecos: { from: number; to: number; deco: Decoration }[] = [];

  for (const { from, to } of view.visibleRanges) {
    const specs = computeLivePreviewSpecs(view.state, from, to);
    for (const spec of specs) {
      switch (spec.kind) {
        case "line":
          lineDecos.push({ line: spec.line, deco: Decoration.line({ class: spec.cls }) });
          break;
        case "mark":
          inlineDecos.push({ from: spec.from, to: spec.to, deco: Decoration.mark({ class: spec.cls }) });
          break;
        case "hide":
          inlineDecos.push({ from: spec.from, to: spec.to, deco: Decoration.replace({}) });
          break;
        case "checkbox":
          inlineDecos.push({
            from: spec.from,
            to: spec.to,
            deco: Decoration.replace({
              widget: new CheckboxWidget(spec.checked, view, spec.from, spec.to),
            }),
          });
          break;
        case "wikilink":
          inlineDecos.push({
            from: spec.from,
            to: spec.to,
            deco: Decoration.replace({
              widget: new WikilinkWidget(spec.target, spec.label, view),
            }),
          });
          break;
        case "hr":
          inlineDecos.push({
            from: spec.from,
            to: spec.to,
            deco: Decoration.replace({ widget: new HrWidget(), block: true }),
          });
          break;
      }
    }
  }

  // Decoration.set(of, sort=true) 自带排序——computeLivePreviewSpecs 的返回
  // 顺序不是全局按 from 排序（wikilink 最后收集），sort:true 兜底。
  const ranges: { from: number; to: number; value: Decoration }[] = inlineDecos.map(
    ({ from, to, deco }) => ({ from, to, value: deco }),
  );
  const doc = view.state.doc;
  lineDecos.sort((a, b) => a.line - b.line);
  for (const { line, deco } of lineDecos) {
    if (line < 1 || line > doc.lines) continue;
    const pos = doc.line(line).from;
    ranges.push({ from: pos, to: pos, value: deco });
  }
  return Decoration.set(ranges, true);
}

class LivePreviewPluginValue {
  decorations: DecorationSet;

  constructor(view: EditorView) {
    this.decorations = buildDecoSet(view);
  }

  update(update: ViewUpdate): void {
    if (update.docChanged || update.selectionSet || update.viewportChanged) {
      this.decorations = buildDecoSet(update.view);
    }
  }
}

export const livePreviewPlugin = ViewPlugin.fromClass(LivePreviewPluginValue, {
  decorations: (v) => v.decorations,
});

/**
 * Live Preview 扩展包：插件 + 主题。
 * 由 MarkdownEditor 经 Compartment 按开关动态装配。
 */
export function livePreview(): Extension {
  return [livePreviewPlugin, livePreviewTheme];
}

export type { DecoSpec } from "./spec";
