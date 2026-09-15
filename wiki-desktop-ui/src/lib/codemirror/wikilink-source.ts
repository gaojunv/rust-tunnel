import type { CompletionContext, CompletionResult, Completion } from "@codemirror/autocomplete";
import { buildInsertion, findLinkQuery } from "@/lib/wikilink-complete";
import { fuzzyScore } from "@/lib/fuzzy";

export type NoteSummaryLike = {
  key: string;
  title: string;
  tags?: string[];
  modified?: number;
};

function rankNotes(
  notes: NoteSummaryLike[],
  query: string,
): NoteSummaryLike[] {
  if (query === "") {
    const withModified = notes.filter((n) => typeof n.modified === "number");
    if (withModified.length > 0) {
      return [...notes]
        .sort((a, b) => (b.modified ?? 0) - (a.modified ?? 0))
        .slice(0, 8);
    }
    return notes.slice(0, 8);
  }
  type Scored = { note: NoteSummaryLike; score: number };
  const scored: Scored[] = [];
  for (const n of notes) {
    const sKey = fuzzyScore(n.key, query);
    const sTitle = fuzzyScore(n.title, query);
    let best: number | null = null;
    if (sKey !== null) best = sKey;
    if (sTitle !== null && (best === null || sTitle > best)) best = sTitle;
    if (best !== null) scored.push({ note: n, score: best });
  }
  scored.sort((a, b) => b.score - a.score || (b.note.modified ?? 0) - (a.note.modified ?? 0));
  return scored.slice(0, 8).map((s) => s.note);
}

export function wikilinkCompletionSource(
  getNotes: () => NoteSummaryLike[],
): (ctx: CompletionContext) => CompletionResult | null {
  return (ctx: CompletionContext): CompletionResult | null => {
    // 只读 caret 所在行：findLinkQuery 仅用行内文本 + caret 行内偏移。
    // caret 大于行尾时（极端竞态）回退为 0，避免 line 内容错位。
    const caret = ctx.pos;
    const line = ctx.state.doc.lineAt(caret);
    const lineText = line.text;
    const caretInLine = caret - line.from;
    const lineInfo = findLinkQuery(lineText, caretInLine);
    if (!lineInfo) return null;
    // findLinkQuery 只见行内文本，其 start 为行内偏移 → 转回文档绝对偏移
    const info = { start: line.from + lineInfo.start, query: lineInfo.query };

    const notes = getNotes();
    const candidates = rankNotes(notes, info.query);
    if (candidates.length === 0) return null;

    const from = info.start;
    const to = caret;

    const options: Completion[] = candidates.map((n) => {
      const label = n.title || n.key.split("/").pop() || n.key;
      const insertion = buildInsertion(n.key, info.query);
      return {
        label,
        detail: n.key,
        apply: insertion,
      };
    });

    return {
      from,
      to,
      options,
      validFor: /^[\p{L}\p{N}/_ -]*$/u,
    };
  };
}
