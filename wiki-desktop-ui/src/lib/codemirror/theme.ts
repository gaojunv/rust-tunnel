import { EditorView } from "@codemirror/view";
import { HighlightStyle, syntaxHighlighting } from "@codemirror/language";
import { tags } from "@lezer/highlight";

/**
 * Wiki CodeMirror theme — consumes the app CSS variables (HSL triplets)
 * so dark/light flips via `.dark` / `html.light` with zero extra wiring.
 */
export const wikiTheme = EditorView.theme({
  "&": {
    backgroundColor: "hsl(var(--background))",
    color: "hsl(var(--foreground))",
    fontFamily:
      'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace',
    fontSize: "0.875rem",
    lineHeight: "1.7",
    height: "100%",
  },
  ".cm-editor": {
    height: "100%",
  },
  ".cm-scroller": {
    overflow: "auto",
  },
  ".cm-content": {
    padding: "8px 0",
    caretColor: "hsl(var(--foreground))",
  },
  ".cm-cursor": {
    borderLeftColor: "hsl(var(--foreground))",
  },
  "&.cm-focused": {
    outline: "none",
  },
  ".cm-selectionBackground, ::selection": {
    backgroundColor: "hsl(var(--muted))",
  },
  "&.cm-focused .cm-selectionBackground": {
    backgroundColor: "hsl(var(--accent))",
  },
  ".cm-activeLine": {
    backgroundColor: "hsl(var(--muted) / 0.4)",
  },
  ".cm-gutters": {
    backgroundColor: "hsl(var(--background))",
    color: "hsl(var(--muted-foreground))",
    borderRight: "1px solid hsl(var(--border))",
  },
  ".cm-tooltip": {
    backgroundColor: "hsl(var(--popover))",
    color: "hsl(var(--popover-foreground))",
    border: "1px solid hsl(var(--border))",
    borderRadius: "0.375rem",
    boxShadow: "0 4px 12px hsl(var(--foreground) / 0.08)",
    overflow: "hidden",
  },
  ".cm-tooltip-autocomplete": {
    "& > ul": {
      fontFamily:
        'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
      fontSize: "0.8125rem",
      maxHeight: "12rem",
    },
    "& > ul > li": {
      padding: "6px 10px",
    },
  },
  ".cm-tooltip-autocomplete ul li[aria-selected]": {
    backgroundColor: "hsl(var(--accent))",
    color: "hsl(var(--accent-foreground))",
  },
  ".cm-completionLabel": {
    fontWeight: "500",
  },
  ".cm-completionDetail": {
    color: "hsl(var(--muted-foreground))",
    fontStyle: "normal",
    marginLeft: "8px",
  },
  ".cm-panel.cm-search": {
    backgroundColor: "hsl(var(--popover))",
    color: "hsl(var(--foreground))",
    borderTop: "1px solid hsl(var(--border))",
    padding: "6px 8px",
    display: "flex",
    alignItems: "center",
    gap: "6px",
    flexWrap: "wrap",
  },
  ".cm-panel.cm-search input": {
    backgroundColor: "hsl(var(--background))",
    color: "hsl(var(--foreground))",
    border: "1px solid hsl(var(--border))",
    borderRadius: "0.375rem",
    padding: "4px 8px",
    fontSize: "0.8125rem",
    outline: "none",
  },
  ".cm-panel.cm-search input:focus": {
    borderColor: "hsl(var(--ring))",
    boxShadow: "0 0 0 2px hsl(var(--ring) / 0.2)",
  },
  ".cm-panel.cm-search button": {
    backgroundColor: "hsl(var(--secondary))",
    color: "hsl(var(--secondary-foreground))",
    border: "1px solid hsl(var(--border))",
    borderRadius: "0.375rem",
    padding: "4px 8px",
    fontSize: "0.8125rem",
    cursor: "pointer",
  },
  ".cm-panel.cm-search button:hover": {
    backgroundColor: "hsl(var(--accent))",
    color: "hsl(var(--accent-foreground))",
  },
  ".cm-panel.cm-search label": {
    fontSize: "0.8125rem",
    display: "inline-flex",
    alignItems: "center",
    gap: "4px",
  },
  ".cm-placeholder": {
    color: "hsl(var(--muted-foreground))",
  },
});

export const wikiHighlightStyle = HighlightStyle.define([
  { tag: tags.heading1, color: "hsl(var(--primary))", fontWeight: "700", fontSize: "1.4em" },
  { tag: tags.heading2, color: "hsl(var(--primary))", fontWeight: "700", fontSize: "1.25em" },
  { tag: tags.heading3, color: "hsl(var(--primary))", fontWeight: "700", fontSize: "1.1em" },
  { tag: tags.heading, color: "hsl(var(--primary))", fontWeight: "700" },
  { tag: tags.strong, fontWeight: "700" },
  { tag: tags.emphasis, fontStyle: "italic" },
  { tag: tags.strikethrough, textDecoration: "line-through" },
  { tag: tags.link, color: "hsl(var(--primary))", textDecoration: "underline", textUnderlineOffset: "2px" },
  { tag: tags.url, color: "hsl(var(--primary))" },
  { tag: tags.quote, color: "hsl(var(--muted-foreground))", fontStyle: "italic" },
  {
    tag: tags.monospace,
    fontFamily:
      'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
    backgroundColor: "hsl(var(--accent) / 0.6)",
    borderRadius: "3px",
    padding: "1px 3px",
  },
  {
    tag: tags.processingInstruction,
    fontFamily:
      'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
    backgroundColor: "hsl(var(--accent) / 0.6)",
    borderRadius: "3px",
    padding: "1px 3px",
  },
  { tag: tags.meta, color: "hsl(var(--muted-foreground))" },
  { tag: tags.list, color: "hsl(var(--muted-foreground))" },
  { tag: tags.comment, color: "hsl(var(--muted-foreground))", fontStyle: "italic" },
]);

export const wikiSyntaxHighlighting = syntaxHighlighting(wikiHighlightStyle);
