import { EditorView } from "@codemirror/view";

/**
 * Live Preview 结构级主题（类名前缀 `cm-lp-`，消费应用 CSS 变量）。
 *
 * 与 `wikiSyntaxHighlighting` 的分层约定：
 * - highlight 层已给 h1-h3 设置字号（em）与内容样式；
 * - 本层 h1-h3 **不设 font-size**（避免与 highlight 生成的 span 叠乘），只加
 *   margin / 边框等结构样式；
 * - h4-h6 在 highlight 层没有字号，用绝对 rem 补齐。
 */
export const livePreviewTheme = EditorView.theme({
  ".cm-lp-h1": {
    marginTop: "0.5em",
    paddingBottom: "0.15em",
    borderBottom: "1px solid hsl(var(--border))",
  },
  ".cm-lp-h2": {
    marginTop: "0.4em",
    paddingBottom: "0.1em",
    borderBottom: "1px solid hsl(var(--border) / 0.6)",
  },
  ".cm-lp-h3": {
    marginTop: "0.3em",
  },
  ".cm-lp-h4": {
    fontSize: "1rem",
    fontWeight: "700",
    marginTop: "0.3em",
  },
  ".cm-lp-h5": {
    fontSize: "0.9rem",
    fontWeight: "700",
    marginTop: "0.25em",
  },
  ".cm-lp-h6": {
    fontSize: "0.8rem",
    fontWeight: "700",
    marginTop: "0.25em",
    color: "hsl(var(--muted-foreground))",
  },
  ".cm-lp-quote": {
    borderLeft: "3px solid hsl(var(--border))",
    paddingLeft: "0.5em",
    color: "hsl(var(--muted-foreground))",
  },
  ".cm-lp-listmark": {
    color: "hsl(var(--muted-foreground))",
    opacity: "0.7",
  },
  ".cm-lp-codeblock-line": {
    backgroundColor: "hsl(var(--muted) / 0.5)",
  },
  ".cm-lp-code-header": {
    display: "flex",
    alignItems: "center",
    padding: "4px 12px",
    background: "hsl(var(--muted) / 0.5)",
    borderBottom: "1px solid hsl(var(--border))",
    borderRadius: "8px 8px 0 0",
    minHeight: "1.6em",
  },
  ".cm-lp-code-lang": {
    fontSize: "0.75em",
    color: "hsl(var(--muted-foreground))",
    fontFamily: "monospace",
  },
  ".cm-lp-code-footer": {
    height: "0",
  },
  ".cm-lp-wikilink": {
    color: "hsl(var(--primary))",
    textDecoration: "underline",
    textUnderlineOffset: "2px",
    cursor: "pointer",
    borderRadius: "3px",
    padding: "0 1px",
  },
  ".cm-lp-wikilink:hover": {
    backgroundColor: "hsl(var(--accent))",
  },
  ".cm-lp-mdlink": {
    color: "hsl(var(--primary))",
    textDecoration: "underline",
    textUnderlineOffset: "2px",
    cursor: "pointer",
    borderRadius: "3px",
    padding: "0 1px",
  },
  ".cm-lp-mdlink:hover": {
    backgroundColor: "hsl(var(--accent))",
  },
  ".cm-lp-checkbox-input": {
    cursor: "pointer",
    marginRight: "0.25em",
    accentColor: "hsl(var(--primary))",
  },
  ".cm-lp-hr": {
    borderTop: "1px solid hsl(var(--border))",
    margin: "0.4em 0",
    height: "0",
  },
  ".cm-lp-image-wrap": {
    display: "inline-block",
    maxWidth: "100%",
    cursor: "pointer",
    lineHeight: "0",
  },
  ".cm-lp-image-block": {
    display: "block",
  },
  ".cm-lp-image": {
    maxWidth: "100%",
    borderRadius: "6px",
    display: "block",
  },
  ".cm-lp-image-ph": {
    display: "inline-block",
    padding: "8px 16px",
    background: "hsl(var(--muted))",
    borderRadius: "6px",
    color: "hsl(var(--muted-foreground))",
    fontSize: "0.85em",
    lineHeight: "1.5",
  },
  ".cm-lp-table": {
    borderCollapse: "collapse",
    width: "100%",
    margin: "0.5em 0",
    fontSize: "0.9em",
  },
  ".cm-lp-table th, .cm-lp-table td": {
    border: "1px solid hsl(var(--border))",
    padding: "6px 13px",
  },
  ".cm-lp-table thead tr": {
    borderBottom: "2px solid hsl(var(--border))",
    background: "hsl(var(--muted))",
  },
  ".cm-lp-table tbody tr:nth-child(even)": {
    background: "hsl(var(--muted) / 0.3)",
  },
});
