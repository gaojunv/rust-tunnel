import type { Extension } from "@codemirror/state";
import {
  Decoration,
  EditorView,
  ViewPlugin,
  ViewUpdate,
  type DecorationSet,
} from "@codemirror/view";
import { computeLivePreviewSpecs, type DecoSpec } from "./spec";
import {
  CheckboxWidget,
  HrWidget,
  WikilinkWidget,
  ImageWidget,
  MdLinkWidget,
  TableWidget,
  CodeHeaderWidget,
  CodeFooterWidget,
  FrontmatterWidget,
} from "./widgets";
import { livePreviewTheme } from "./theme";
import { isAttachmentSrc } from "@/lib/attachments";

/**
 * 判断 image spec 的 src 是否是附件路径（由 spec.ts 中 normalizeAttachmentSrc 处理过）。
 * spec.ts 会将附件路径 normalize 为 `assets/...` 格式，普通 URL 不会命中。
 * 此处直接调用 isAttachmentSrc 检测——规范化后的路径仍以 `assets/` 开头，
 * isAttachmentSrc 不会对 http(s)/data: URL 返回 true。
 */
function isImageAttachmentSpec(src: string): boolean {
  return isAttachmentSrc(src);
}

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
 * - `checkbox` / `hr` / `wikilink` / `image` / `mdlink` / `table` / `codeheader` / `codefooter`
 *   → `Decoration.replace({ widget })`
 *
 * 隐藏区间与 widget 区间必须避免部分重叠：同一标题的 HeaderMark 分属不同
 * 区间，彼此不交叠；checkbox 与同行 ListMark 区间分离（`[ ]` vs `-`）。
 * 同一起点（如行首开标记与行装饰）由不同装饰通道承载，无冲突。
 */
function buildDecoSet(view: EditorView): { decos: DecorationSet; specs: DecoSpec[] } {
  const lineDecos: { line: number; deco: Decoration }[] = [];
  const inlineDecos: { from: number; to: number; deco: Decoration }[] = [];
  const allSpecs: DecoSpec[] = [];

  for (const { from, to } of view.visibleRanges) {
    const specs = computeLivePreviewSpecs(view.state, from, to);
    allSpecs.push(...specs);
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
        case "image":
          inlineDecos.push({
            from: spec.from,
            to: spec.to,
            deco: Decoration.replace({
              widget: new ImageWidget(
                spec.alt,
                spec.src,
                isImageAttachmentSpec(spec.src),
                view,
                spec.from,
                spec.block,
              ),
              block: spec.block,
            }),
          });
          break;
        case "mdlink":
          inlineDecos.push({
            from: spec.from,
            to: spec.to,
            deco: Decoration.replace({
              widget: new MdLinkWidget(spec.text, spec.url, view),
            }),
          });
          break;
        case "table":
          inlineDecos.push({
            from: spec.from,
            to: spec.to,
            deco: Decoration.replace({ widget: new TableWidget(spec.raw), block: true }),
          });
          break;
        case "codeheader":
          inlineDecos.push({
            from: spec.from,
            to: spec.to,
            deco: Decoration.replace({ widget: new CodeHeaderWidget(spec.lang), block: true }),
          });
          break;
        case "codefooter":
          inlineDecos.push({
            from: spec.from,
            to: spec.to,
            deco: Decoration.replace({ widget: new CodeFooterWidget(), block: true }),
          });
          break;
        case "frontmatter":
          inlineDecos.push({
            from: spec.from,
            to: spec.to,
            deco: Decoration.replace({
              widget: new FrontmatterWidget(spec.propCount, view, spec.from),
              block: true,
            }),
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
  return { decos: Decoration.set(ranges, true), specs: allSpecs };
}

class LivePreviewPluginValue {
  decorations: DecorationSet;
  /** 最近一次 buildDecoSet 的 specs，供 domEventHandlers 读取 */
  specs: DecoSpec[] = [];

  constructor(view: EditorView) {
    const result = buildDecoSet(view);
    this.decorations = result.decos;
    this.specs = result.specs;
  }

  update(update: ViewUpdate): void {
    if (update.docChanged || update.selectionSet || update.viewportChanged) {
      const result = buildDecoSet(update.view);
      this.decorations = result.decos;
      this.specs = result.specs;
    }
  }
}

/**
 * Cmd/Ctrl + 点击命中 mdlink → window.open(url)。
 * 命中 image → 若为 http(s)/data: URL 也打开。
 */
function handleModClick(e: MouseEvent, view: EditorView): boolean {
  if (!(e.metaKey || e.ctrlKey)) return false;
  const plugin = view.plugin(livePreviewPlugin);
  if (!plugin) return false;
  const pos = view.posAtCoords({ x: e.clientX, y: e.clientY }, false);
  if (pos == null) return false;
  for (const spec of plugin.specs) {
    if (spec.kind === "line") continue;
    if (pos < spec.from || pos >= spec.to) continue;
    if (spec.kind === "mdlink") {
      try {
        window.open(spec.url, "_blank");
      } catch {
        // ignore — invalid URL
      }
      e.preventDefault();
      return true;
    }
    if (spec.kind === "image") {
      if (
        spec.src.startsWith("http://") ||
        spec.src.startsWith("https://") ||
        spec.src.startsWith("data:")
      ) {
        try {
          window.open(spec.src, "_blank");
        } catch {
          // ignore
        }
        e.preventDefault();
        return true;
      }
      break; // attachment — don't open externally
    }
  }
  return false;
}

export const livePreviewPlugin = ViewPlugin.fromClass(LivePreviewPluginValue, {
  decorations: (v) => v.decorations,
  eventHandlers: {
    click: handleModClick,
  },
});

/**
 * Live Preview 扩展包：插件 + 主题。
 * 由 MarkdownEditor 经 Compartment 按开关动态装配。
 */
export function livePreview(): Extension {
  return [livePreviewPlugin, livePreviewTheme];
}

export type { DecoSpec } from "./spec";
