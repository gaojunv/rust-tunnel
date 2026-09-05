import type { Command } from "@codemirror/view";
import { EditorView } from "@codemirror/view";
import { EditorState, type ChangeSpec } from "@codemirror/state";

/* ------------------------------------------------------------------ */
/*  pure helpers                                                       */
/* ------------------------------------------------------------------ */

const PLACEHOLDER_MAP: Record<string, string> = {
  "**": "粗体",
  "*": "斜体",
  "~~": "删除线",
  "`": "代码",
};

export type PureResult = {
  changes: ChangeSpec | readonly ChangeSpec[];
  selection: { anchor: number; head?: number };
} | null;

/**
 * Core inline marker toggle — pure, works on EditorState only.
 * Handles wrap/unwrap for non-empty selections, placeholder for empty,
 * and unwrapping when cursor is inside an existing wrapped span.
 */
export function toggleInlineMarker(
  state: EditorState,
  marker: string,
  placeholder?: string,
): PureResult {
  const ph = placeholder ?? PLACEHOLDER_MAP[marker] ?? "文本";
  const doc = state.doc.toString();
  const sel = state.selection.main;
  const from = sel.from;
  const to = sel.to;
  const mLen = marker.length;

  // --- non-empty selection -------------------------------------------------
  if (from !== to) {
    const selectedText = doc.slice(from, to);

    // case A: selection itself is wrapped (user selected with markers)
    if (
      selectedText.length >= mLen * 2 &&
      selectedText.startsWith(marker) &&
      selectedText.endsWith(marker)
    ) {
      // for single "*" ensure not part of bold
      if (marker === "*" && isBoldBoundary(selectedText)) {
        // fall through to wrap logic
      } else {
        const inner = selectedText.slice(mLen, selectedText.length - mLen);
        return {
          changes: { from, to, insert: inner },
          selection: { anchor: from, head: from + inner.length },
        };
      }
    }

    // case B: markers immediately outside selection
    if (isWrappedOutside(doc, from, to, marker)) {
      return {
        changes: { from: from - mLen, to: to + mLen, insert: selectedText },
        selection: { anchor: from - mLen, head: from - mLen + selectedText.length },
      };
    }

    // otherwise wrap
    return {
      changes: { from, to, insert: marker + selectedText + marker },
      selection: { anchor: from + mLen, head: to + mLen },
    };
  }

  // --- empty selection -----------------------------------------------------
  const enclosing = findEnclosingMarkers(doc, from, marker);
  if (enclosing) {
    const { openPos, closePos } = enclosing;
    const inner = doc.slice(openPos + mLen, closePos);
    return {
      changes: { from: openPos, to: closePos + mLen, insert: inner },
      selection: { anchor: from - mLen, head: from - mLen },
    };
  }

  return {
    changes: { from, to, insert: marker + ph + marker },
    selection: { anchor: from + mLen, head: from + mLen + ph.length },
  };
}

function isBoldBoundary(text: string): boolean {
  return text.startsWith("**") || text.endsWith("**");
}

function isWrappedOutside(doc: string, from: number, to: number, marker: string): boolean {
  const mLen = marker.length;
  if (from < mLen) return false;
  if (to + mLen > doc.length) return false;
  const before = doc.slice(from - mLen, from);
  const after = doc.slice(to, to + mLen);
  if (before !== marker || after !== marker) return false;
  if (marker === "*") {
    // single star must not be part of "**"
    const beforeDoubleLeft = from >= 2 && doc.slice(from - 2, from) === "**";
    const afterDouble = doc.slice(to, to + 2) === "**";
    if (beforeDoubleLeft || afterDouble) return false;
    // also ensure chars beyond markers are not "*"
    const charBeforeOuter = from - mLen - 1 >= 0 ? doc[from - mLen - 1] : "";
    const charAfterOuter = to + mLen < doc.length ? doc[to + mLen] : "";
    if (charBeforeOuter === "*" || charAfterOuter === "*") return false;
  }
  return true;
}

function findEnclosingMarkers(
  doc: string,
  pos: number,
  marker: string,
): { openPos: number; closePos: number } | null {
  const mLen = marker.length;
  if (marker === "*") return findEnclosingSingleStar(doc, pos);
  const openPos = doc.lastIndexOf(marker, pos - 1);
  if (openPos === -1) return null;
  if (openPos + mLen > pos) return null;
  const closePos = doc.indexOf(marker, pos);
  if (closePos === -1) return null;
  if (closePos < pos) return null;
  if (pos < openPos + mLen || pos > closePos) return null;
  const inner = doc.slice(openPos + mLen, closePos);
  if (inner.includes("\n")) return null;
  if (inner.includes(marker)) return null;
  return { openPos, closePos };
}

function findEnclosingSingleStar(
  doc: string,
  pos: number,
): { openPos: number; closePos: number } | null {
  let openPos: number | null = null;
  for (let i = pos - 1; i >= 0; i--) {
    if (doc[i] === "*") {
      const isDoubleLeft = i > 0 && doc[i - 1] === "*";
      const isDoubleRight = i + 1 < doc.length && doc[i + 1] === "*";
      if (isDoubleLeft || isDoubleRight) continue;
      openPos = i;
      break;
    }
  }
  if (openPos === null) return null;
  let closePos: number | null = null;
  for (let i = pos; i < doc.length; i++) {
    if (doc[i] === "*") {
      const isDoubleLeft = i > 0 && doc[i - 1] === "*";
      const isDoubleRight = i + 1 < doc.length && doc[i + 1] === "*";
      if (isDoubleLeft || isDoubleRight) continue;
      closePos = i;
      break;
    }
  }
  if (closePos === null) return null;
  if (openPos >= closePos) return null;
  if (pos <= openPos || pos > closePos) return null;
  const inner = doc.slice(openPos + 1, closePos);
  if (inner.includes("\n") || inner.length === 0) return null;
  if (inner.includes("*")) return null;
  return { openPos, closePos };
}

/* ------------------------------------------------------------------ */
/*  heading                                                             */
/* ------------------------------------------------------------------ */

export function setHeadingOnLines(state: EditorState, level: 1 | 2 | 3): PureResult {
  const doc = state.doc;
  const sel = state.selection.main;
  const fromLine = doc.lineAt(sel.from);
  let toLine = doc.lineAt(sel.to);
  if (sel.to === toLine.from && sel.from !== sel.to) {
    toLine = doc.lineAt(Math.max(0, sel.to - 1));
  }
  const startNum = fromLine.number;
  const endNum = toLine.number;

  type LInfo = { lineFrom: number; lineTo: number; text: string; stripped: string; lvl: number };
  const infos: LInfo[] = [];
  for (let n = startNum; n <= endNum; n++) {
    const line = doc.line(n);
    const m = line.text.match(/^(#{1,6})\s+/);
    const lvl = m ? m[1].length : 0;
    const stripped = line.text.replace(/^(#{1,6})\s+/, "");
    infos.push({ lineFrom: line.from, lineTo: line.to, text: line.text, stripped, lvl });
  }

  const allSameLevel = infos.every((info) => info.lvl === level);
  const changes: ChangeSpec[] = [];
  const newTexts: string[] = [];
  for (const info of infos) {
    const newText = allSameLevel ? info.stripped : `${"#".repeat(level)} ${info.stripped}`;
    newTexts.push(newText);
    changes.push({ from: info.lineFrom, to: info.lineTo, insert: newText });
  }

  let newAnchor = sel.anchor;
  let newHead = sel.head;
  for (let i = 0; i < infos.length; i++) {
    const info = infos[i];
    const delta = newTexts[i].length - info.text.length;
    const lineNum = startNum + i;
    const anchorLineNum = doc.lineAt(sel.anchor).number;
    const headLineNum = doc.lineAt(sel.head).number;
    if (lineNum < anchorLineNum) newAnchor += delta;
    else if (lineNum === anchorLineNum && sel.anchor > info.lineFrom) newAnchor += delta;
    if (lineNum < headLineNum) newHead += delta;
    else if (lineNum === headLineNum && sel.head > info.lineFrom) newHead += delta;
  }

  return { changes, selection: { anchor: newAnchor, head: newHead } };
}

/* ------------------------------------------------------------------ */
/*  lists                                                               */
/* ------------------------------------------------------------------ */

export type ListKind = "bullet" | "ordered" | "task";

function hasBullet(rest: string): boolean {
  return rest.startsWith("- ") && !/^-\s*\[[ xX]\]\s/.test(rest);
}
function hasOrdered(rest: string): boolean {
  return /^\d+\.\s/.test(rest);
}
function hasTask(rest: string): boolean {
  return /^-\s*\[[ xX]\]\s/.test(rest);
}

function stripAnyListPrefix(rest: string): string {
  if (hasTask(rest)) return rest.replace(/^-\s*\[[ xX]\]\s+/, "");
  if (hasOrdered(rest)) return rest.replace(/^\d+\.\s+/, "");
  if (hasBullet(rest)) return rest.slice(2);
  return rest;
}

export function setListOnLines(state: EditorState, kind: ListKind): PureResult {
  const doc = state.doc;
  const sel = state.selection.main;
  const fromLine = doc.lineAt(sel.from);
  let toLine = doc.lineAt(sel.to);
  if (sel.to === toLine.from && sel.from !== sel.to) {
    toLine = doc.lineAt(Math.max(0, sel.to - 1));
  }
  const startNum = fromLine.number;
  const endNum = toLine.number;

  type Info = { lineFrom: number; lineTo: number; text: string; ws: string; rest: string };
  const infos: Info[] = [];
  for (let n = startNum; n <= endNum; n++) {
    const line = doc.line(n);
    const ws = (line.text.match(/^\s*/) ?? [""])[0];
    const rest = line.text.slice(ws.length);
    infos.push({ lineFrom: line.from, lineTo: line.to, text: line.text, ws, rest });
  }

  let allHaveKind = true;
  for (const info of infos) {
    if (kind === "bullet" && !hasBullet(info.rest)) allHaveKind = false;
    if (kind === "ordered" && !hasOrdered(info.rest)) allHaveKind = false;
    if (kind === "task" && !hasTask(info.rest)) allHaveKind = false;
  }

  const changes: ChangeSpec[] = [];
  const newTexts: string[] = [];

  if (kind === "ordered") {
    let counter = 1;
    for (const info of infos) {
      let newRest: string;
      if (allHaveKind) {
        newRest = info.rest.replace(/^\d+\.\s+/, "");
      } else {
        if (hasOrdered(info.rest)) {
          newRest = info.rest.replace(/^\d+\.\s+/, `${counter}. `);
        } else {
          const stripped = stripAnyListPrefix(info.rest);
          newRest = `${counter}. ${stripped}`;
        }
        counter++;
      }
      const newText = info.ws + newRest;
      newTexts.push(newText);
      changes.push({ from: info.lineFrom, to: info.lineTo, insert: newText });
    }
  } else if (kind === "bullet") {
    for (const info of infos) {
      let newRest: string;
      if (allHaveKind) {
        newRest = info.rest.slice(2);
      } else {
        if (hasBullet(info.rest)) newRest = info.rest;
        else {
          const stripped = stripAnyListPrefix(info.rest);
          newRest = `- ${stripped}`;
        }
      }
      const newText = info.ws + newRest;
      newTexts.push(newText);
      changes.push({ from: info.lineFrom, to: info.lineTo, insert: newText });
    }
  } else {
    // task
    for (const info of infos) {
      let newRest: string;
      if (allHaveKind) {
        newRest = info.rest.replace(/^-\s*\[[ xX]\]\s+/, "");
      } else {
        if (hasTask(info.rest)) newRest = info.rest;
        else {
          const stripped = stripAnyListPrefix(info.rest);
          newRest = `- [ ] ${stripped}`;
        }
      }
      const newText = info.ws + newRest;
      newTexts.push(newText);
      changes.push({ from: info.lineFrom, to: info.lineTo, insert: newText });
    }
  }

  let newAnchor = sel.anchor;
  let newHead = sel.head;
  for (let i = 0; i < infos.length; i++) {
    const info = infos[i];
    const delta = newTexts[i].length - info.text.length;
    const lineNum = startNum + i;
    const anchorLineNum = doc.lineAt(sel.anchor).number;
    const headLineNum = doc.lineAt(sel.head).number;
    if (lineNum < anchorLineNum) newAnchor += delta;
    else if (lineNum === anchorLineNum && sel.anchor > info.lineFrom) newAnchor += delta;
    if (lineNum < headLineNum) newHead += delta;
    else if (lineNum === headLineNum && sel.head > info.lineFrom) newHead += delta;
  }

  return { changes, selection: { anchor: newAnchor, head: newHead } };
}

/* ------------------------------------------------------------------ */
/*  table / link / image                                              */
/* ------------------------------------------------------------------ */

export function insertTablePure(state: EditorState): PureResult {
  const sel = state.selection.main;
  const doc = state.doc;
  const line = doc.lineAt(sel.from);
  const atLineStart = sel.from === line.from && sel.to === line.from;
  const isEmptyLine = line.text.trim() === "";
  const needBlankLine = !atLineStart && !isEmptyLine;

  const table = "| Header 1 | Header 2 | Header 3 |\n| --- | --- | --- |\n|  |  |  |\n|  |  |  |";
  const prefix = needBlankLine ? "\n\n" : "";
  const insert = prefix + table;
  const from = sel.from;
  const to = sel.to;
  const cellOffset = prefix.length + 2; // "| " length
  const headerLen = "Header 1".length;
  return {
    changes: { from, to, insert },
    selection: { anchor: from + cellOffset, head: from + cellOffset + headerLen },
  };
}

export function insertLinkPure(state: EditorState): PureResult {
  const sel = state.selection.main;
  const from = sel.from;
  const to = sel.to;
  const selected = state.doc.sliceString(from, to);
  if (selected.length > 0) {
    const insert = `[${selected}](url)`;
    const urlStart = from + 1 + selected.length + 2;
    return {
      changes: { from, to, insert },
      selection: { anchor: urlStart, head: urlStart + 3 },
    };
  } else {
    const insert = `[标题](url)`;
    return {
      changes: { from, to, insert },
      selection: { anchor: from + 1, head: from + 1 + 2 },
    };
  }
}

export function insertImageMarkdown(path: string, alt?: string): string {
  const a = alt ?? "image";
  return `![${a}](${path})`;
}

/* ------------------------------------------------------------------ */
/*  Command wrappers                                                  */
/* ------------------------------------------------------------------ */

function makeInlineCommand(marker: string, placeholder?: string): Command {
  return (view: EditorView): boolean => {
    const result = toggleInlineMarker(view.state, marker, placeholder);
    if (!result) return false;
    view.dispatch({ changes: result.changes, selection: result.selection });
    return true;
  };
}

export const toggleBold: Command = makeInlineCommand("**", "粗体");
export const toggleItalic: Command = makeInlineCommand("*", "斜体");
export const toggleStrikethrough: Command = makeInlineCommand("~~", "删除线");
export const toggleInlineCode: Command = makeInlineCommand("`", "代码");

export function toggleHeading(level: 1 | 2 | 3): Command {
  return (view: EditorView): boolean => {
    const result = setHeadingOnLines(view.state, level);
    if (!result) return false;
    view.dispatch({ changes: result.changes, selection: result.selection });
    return true;
  };
}

export function toggleBulletListCommand(view: EditorView): boolean {
  const result = setListOnLines(view.state, "bullet");
  if (!result) return false;
  view.dispatch({ changes: result.changes, selection: result.selection });
  return true;
}
export function toggleOrderedListCommand(view: EditorView): boolean {
  const result = setListOnLines(view.state, "ordered");
  if (!result) return false;
  view.dispatch({ changes: result.changes, selection: result.selection });
  return true;
}
export function toggleTaskListCommand(view: EditorView): boolean {
  const result = setListOnLines(view.state, "task");
  if (!result) return false;
  view.dispatch({ changes: result.changes, selection: result.selection });
  return true;
}

export const toggleBulletList: Command = toggleBulletListCommand;
export const toggleOrderedList: Command = toggleOrderedListCommand;
export const toggleTaskList: Command = toggleTaskListCommand;

export const insertTable: Command = (view: EditorView): boolean => {
  const result = insertTablePure(view.state);
  if (!result) return false;
  view.dispatch({ changes: result.changes, selection: result.selection });
  return true;
};

export const insertLink: Command = (view: EditorView): boolean => {
  const result = insertLinkPure(view.state);
  if (!result) return false;
  view.dispatch({ changes: result.changes, selection: result.selection });
  return true;
};
