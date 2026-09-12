import { StateField, type EditorState, type Extension, type Transaction } from "@codemirror/state";
import {
  Decoration,
  EditorView,
  type DecorationSet,
} from "@codemirror/view";
import {
  computeZonedSpecs,
  filterZonedSpecs,
  type DecoSpec,
  type ZonedSpec,
} from "./spec";
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
  CalloutWidget,
  MathWidget,
  BulletWidget,
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
 * **为什么用 StateField 而不是 ViewPlugin**：CM6 规定块级装饰
 * （`block: true` 的 replace widget：hr/table/codeheader/codefooter/
 * frontmatter/mathblock/块级图片）不允许由 ViewPlugin 提供
 * （"Block decorations may not be specified via plugins"），
 * 必须经 StateField/facet 提供。因此装饰集存在 StateField 里，
 * 通过 `EditorView.decorations.from` 暴露；widget 不持有 view，
 * 交互一律走 `toDOM(view)` 入参。
 *
 * - 对整篇文档调用纯函数核心（不再按 visibleRanges 分段——StateField
 *   拿不到 viewport；笔记体量下全文计算足够快）；
 * - `docChanged || selection` 时重算（`syntaxTree` 增量解析）；
 * - 排序：行装饰用 `line()`；行内按 `from` 递增建集，`Decoration.set(sort: true)` 兜底。
 *
 * 映射约定：
 * - `line` → `Decoration.line`
 * - `mark` → `Decoration.mark`
 * - `hide` → `Decoration.replace({})`（零宽，不占位）
 * - `checkbox` / `hr` / `wikilink` / `image` / `mdlink` / `table` / `codeheader` / `codefooter` / `callout` / `mathblock` / `mathinline` / `bullet`
 *   → `Decoration.replace({ widget })`
 * - `highlight` → 两个 `==` 分别 `Decoration.replace({})`，内容 `Decoration.mark`
 */
/**
 * 由可见 spec 集构建装饰（纯映射，不做计算）。
 * 选区变化时只重跑 `filterZonedSpecs` + 本函数，不再重走语法树/行扫描。
 */
function buildDecoSetFromSpecs(
  state: EditorState,
  specs: readonly DecoSpec[],
): { decos: DecorationSet; specs: DecoSpec[] } {
  const lineDecos: { line: number; deco: Decoration }[] = [];
  const inlineDecos: { from: number; to: number; deco: Decoration }[] = [];
  const allSpecs: DecoSpec[] = [];

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
            widget: new CheckboxWidget(spec.checked, spec.from, spec.to),
          }),
        });
        break;
      case "wikilink":
        inlineDecos.push({
          from: spec.from,
          to: spec.to,
          deco: Decoration.replace({
            widget: new WikilinkWidget(spec.target, spec.label),
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
              spec.from,
              spec.block,
              spec.width,
              spec.height,
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
            widget: new MdLinkWidget(spec.text, spec.url),
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
      case "callout":
        inlineDecos.push({
          from: spec.from,
          to: spec.to,
          deco: Decoration.replace({
            widget: new CalloutWidget(spec.calloutType, spec.title),
          }),
        });
        break;
      case "highlight":
        inlineDecos.push({
          from: spec.markFrom,
          to: spec.markFrom + 2,
          deco: Decoration.replace({}),
        });
        inlineDecos.push({
          from: spec.markTo,
          to: spec.markTo + 2,
          deco: Decoration.replace({}),
        });
        inlineDecos.push({
          from: spec.from,
          to: spec.to,
          deco: Decoration.mark({ class: "cm-lp-highlight" }),
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
            widget: new FrontmatterWidget(spec.propCount, spec.from),
            block: true,
          }),
        });
        break;
      case "mathblock":
        inlineDecos.push({
          from: spec.from,
          to: spec.to,
          deco: Decoration.replace({
            widget: new MathWidget(spec.tex, true, spec.from),
            block: true,
          }),
        });
        break;
      case "mathinline":
        inlineDecos.push({
          from: spec.from,
          to: spec.to,
          deco: Decoration.replace({
            widget: new MathWidget(spec.tex, false, spec.from),
          }),
        });
        break;
      case "bullet":
        inlineDecos.push({
          from: spec.from,
          to: spec.to,
          deco: Decoration.replace({ widget: new BulletWidget() }),
        });
        break;
    }
  }

  // Decoration.set(of, sort=true) 自带排序——spec 集的返回
  // 顺序不是全局按 from 排序（wikilink 最后收集），sort:true 兜底。
  const ranges: { from: number; to: number; value: Decoration }[] = inlineDecos.map(
    ({ from, to, deco }) => ({ from, to, value: deco }),
  );
  const doc = state.doc;
  lineDecos.sort((a, b) => a.line - b.line);
  for (const { line, deco } of lineDecos) {
    if (line < 1 || line > doc.lines) continue;
    const pos = doc.line(line).from;
    ranges.push({ from: pos, to: pos, value: deco });
  }
  return { decos: Decoration.set(ranges, true), specs: allSpecs };
}

interface LivePreviewState {
  decos: DecorationSet;
  /** 最近一次构建的可见 specs，供 Mod+Click 处理读取 */
  specs: DecoSpec[];
  /** 文档内容对应的选择无关 spec 全集（文档不变时复用，选区变化只重过滤） */
  zoned: ZonedSpec[];
  /** 构建 zoned 时的文档对象（Text 不可变，引用比较即可判断文档是否变化） */
  doc: EditorState["doc"];
}

/** 全量构建（含语法树遍历 + 行扫描），只在文档变化时调用。 */
function rebuildAll(state: EditorState): LivePreviewState {
  const zoned = computeZonedSpecs(state);
  const specs = filterZonedSpecs(zoned, state.selection.ranges);
  const { decos } = buildDecoSetFromSpecs(state, specs);
  return { decos, specs, zoned, doc: state.doc };
}

/** 选区变化时增量更新：复用 zoned 全集，只重做过滤 + 装饰映射。 */
function updateForSelection(value: LivePreviewState, state: EditorState): LivePreviewState {
  const specs = filterZonedSpecs(value.zoned, state.selection.ranges);
  const { decos } = buildDecoSetFromSpecs(state, specs);
  return { decos, specs, zoned: value.zoned, doc: value.doc };
}

/**
 * Live Preview 装饰 StateField。
 * - 文档变化时全量重算（语法树遍历 + 行扫描 + 过滤 + 装饰映射）；
 * - 仅选区变化时增量更新（复用缓存的全集，只重做过滤 + 装饰映射）；
 * - 文档与选区均未变时原样保留。
 * 注意 `tr.selection` 是新选区对象（恒真值），必须用新旧选区比较判断变化。
 */
export const livePreviewField = StateField.define<LivePreviewState>({
  create(state) {
    return rebuildAll(state);
  },
  update(value, tr: Transaction) {
    if (tr.docChanged || tr.state.doc !== value.doc) return rebuildAll(tr.state);
    if (!tr.startState.selection.eq(tr.state.selection)) {
      return updateForSelection(value, tr.state);
    }
    return value;
  },
  provide: (f) => EditorView.decorations.from(f, (v) => v.decos),
});

/**
 * Cmd/Ctrl + 点击命中 mdlink → window.open(url)。
 * 命中 image → 若为 http(s)/data: URL 也打开。
 */
function handleModClick(e: MouseEvent, view: EditorView): boolean {
  if (!(e.metaKey || e.ctrlKey)) return false;
  const field = view.state.field(livePreviewField, false);
  if (!field) return false;
  const pos = view.posAtCoords({ x: e.clientX, y: e.clientY }, false);
  if (pos == null) return false;
  for (const spec of field.specs) {
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

const livePreviewClickHandlers = EditorView.domEventHandlers({
  click: handleModClick,
});

/**
 * Live Preview 扩展包：StateField + 事件处理 + 主题。
 * 由 MarkdownEditor 经 Compartment 按开关动态装配。
 */
export function livePreview(): Extension {
  return [livePreviewField, livePreviewClickHandlers, livePreviewTheme];
}

export type { DecoSpec, ZonedSpec, RevealZone } from "./spec";
