//! Markdown 渲染与结构提取。

use crate::link::WikiLink;

/// 一次 Markdown 渲染的产物。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownDoc {
    /// 渲染出的 HTML。
    pub html: String,
    /// 正文中提取到的 wiki 链接。
    pub wikilinks: Vec<WikiLink>,
    /// 标题层级文本（`#`/`##`/`###` …）。
    pub headings: Vec<String>,
}

fn comrak_options<'a>() -> comrak::Options<'a> {
    let mut options = comrak::Options::default();
    options.extension.wikilinks_title_after_pipe = true;
    options.extension.front_matter_delimiter = Some("---".to_owned());
    options
}

fn postprocess_wikilinks(html: &str) -> String {
    let needle = " data-wikilink=\"true\"";
    let mut result = String::with_capacity(html.len().saturating_add(32));
    let mut last = 0usize;
    let mut search_start = 0usize;
    while let Some(rel) = html[search_start..].find(needle) {
        let needle_start = search_start + rel;
        let needle_end = needle_start + needle.len();
        let segment_before = &html[last..needle_start];
        if let Some(href_rel) = segment_before.rfind("href=\"") {
            let href_abs = last + href_rel;
            let value_start = href_abs + 6;
            result.push_str(&html[last..value_start]);
            result.push_str("wikilink:");
            result.push_str(&html[value_start..needle_start]);
            last = needle_end;
            search_start = needle_end;
        } else {
            result.push_str(&html[last..needle_end]);
            last = needle_end;
            search_start = needle_end;
        }
    }
    result.push_str(&html[last..]);
    result
}

/// 用 comrak 将 Markdown 渲染为 HTML。
///
/// 开启 `wikilinks_title_after_pipe` 与 `front_matter_delimiter = "---"`；
/// WikiLink 渲染为 `<a href="wikilink:target">显示名</a>` 形态。
#[must_use]
pub fn render_html(content: &str) -> String {
    let options = comrak_options();
    let raw = comrak::markdown_to_html(content, &options);
    postprocess_wikilinks(&raw)
}

/// 单次 comrak 解析的产物：代码块字节区间与标题文本。
///
/// 由 [`analyze_markdown`] 一次 `parse_document` 产出，供调用方复用，
/// 避免对同一正文做多次全量解析。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownAnalysis {
    /// 代码块（围栏块与行内码）的字节区间（开区间 `end`）。
    pub code_ranges: Vec<(usize, usize)>,
    /// 标题层级文本（`#`/`##`/`###` …）。
    pub headings: Vec<String>,
}

/// 单次 comrak 解析同时产出代码块区间与标题。
///
/// 只调一次 `parse_document`，在同一 AST 上做线性遍历收集两种信息；
/// 等价于 `collect_code_ranges` + [`extract_headings`] 分两次解析的结果。
#[must_use]
pub fn analyze_markdown(content: &str) -> MarkdownAnalysis {
    use comrak::nodes::NodeValue;
    use comrak::Arena;

    let options = comrak_options();
    let arena = Arena::new();
    let root = comrak::parse_document(&arena, content, &options);
    let line_starts = compute_line_starts(content);
    let content_len = content.len();
    let mut code_ranges = Vec::new();
    let mut headings = Vec::new();
    for node in root.descendants() {
        let ast = node.data();
        match &ast.value {
            NodeValue::CodeBlock(_) | NodeValue::Code(_) => {
                let sp = ast.sourcepos;
                if sp.start.line == 0 || sp.end.line == 0 {
                    continue;
                }
                let Some(start) =
                    byte_offset(&line_starts, content_len, sp.start.line, sp.start.column)
                else {
                    continue;
                };
                let Some(end_inclusive) =
                    byte_offset(&line_starts, content_len, sp.end.line, sp.end.column)
                else {
                    continue;
                };
                let end_exclusive = end_inclusive.checked_add(1).unwrap_or(content_len);
                let end_clamped = if end_exclusive > content_len {
                    content_len
                } else {
                    end_exclusive
                };
                if start < end_clamped {
                    code_ranges.push((start, end_clamped));
                }
            }
            NodeValue::Heading(_) => {
                let mut text = String::new();
                for desc in node.descendants() {
                    if desc.same_node(node) {
                        continue;
                    }
                    let d = desc.data();
                    match &d.value {
                        NodeValue::Text(t) => text.push_str(t),
                        NodeValue::Code(c) => text.push_str(&c.literal),
                        _ => {}
                    }
                }
                let trimmed = text.trim().to_owned();
                if trimmed.is_empty() {
                    // 空标题仍记录为空串以保留结构，但通常不出现
                    headings.push(String::new());
                } else {
                    headings.push(trimmed);
                }
            }
            _ => {}
        }
    }
    MarkdownAnalysis {
        code_ranges,
        headings,
    }
}

/// 提取所有标题的文本（跳过 frontmatter）。
///
/// 经 [`analyze_markdown`] 单次解析产出，与旧实现（独立 `parse_document`）
/// 结果一致。
#[must_use]
pub fn extract_headings(content: &str) -> Vec<String> {
    analyze_markdown(content).headings
}

/// 收集正文中代码块的字节区间（单次解析）。
///
/// 经 [`analyze_markdown`] 产出，供 [`crate::link`] 复用，避免重复解析。
#[must_use]
pub(crate) fn code_ranges(content: &str) -> Vec<(usize, usize)> {
    analyze_markdown(content).code_ranges
}

fn compute_line_starts(content: &str) -> Vec<usize> {
    let mut starts = Vec::with_capacity(32);
    starts.push(0);
    for (idx, byte) in content.bytes().enumerate() {
        if byte == b'\n' && idx < content.len() {
            starts.push(idx + 1);
        }
    }
    starts
}

fn byte_offset(
    line_starts: &[usize],
    content_len: usize,
    line: usize,
    column: usize,
) -> Option<usize> {
    if line == 0 || column == 0 {
        return None;
    }
    let line_idx = line.checked_sub(1)?;
    let line_start = *line_starts.get(line_idx)?;
    let offset = line_start.checked_add(column.checked_sub(1)?)?;
    if offset > content_len {
        return None;
    }
    Some(offset)
}

/// 去掉 frontmatter 后返回正文部分。
///
/// 识别文档开头的 `---` 定界块（`---\n...\n---\n`），返回其后的切片；
/// 无 frontmatter 时原样返回。
#[must_use]
pub fn strip_frontmatter(content: &str) -> &str {
    if content.is_empty() {
        return content;
    }
    let bytes = content.as_bytes();
    if bytes.len() < 3 || &bytes[..3] != b"---" {
        return content;
    }
    // 首行必须是 `---`（可带尾随空白）且后跟换行
    let first_line_end = content.find('\n');
    let Some(first_nl) = first_line_end else {
        return content;
    };
    let first_line = &content[..first_nl];
    if first_line.trim() != "---" {
        return content;
    }
    let rest = &content[first_nl + 1..];
    // 逐行寻找关闭分隔符
    let offset = first_nl + 1;
    let mut search_pos = 0usize;
    while search_pos < rest.len() {
        let line_end = rest[search_pos..].find('\n');
        let (line, line_len) = if let Some(rel) = line_end {
            (&rest[search_pos..search_pos + rel], rel + 1)
        } else {
            (&rest[search_pos..], rest.len() - search_pos)
        };
        // 去掉可能的 `\r`
        let trimmed = line.trim_end_matches('\r').trim();
        if trimmed == "---" {
            let end = offset + search_pos + line_len;
            let remaining = if end <= content.len() {
                &content[end..]
            } else {
                ""
            };
            // 去掉关闭分隔符后的首个空行（单个换行/回车换行组合）
            let stripped = remaining.trim_start_matches(['\n', '\r']);
            // 若原内容在关闭分隔符后仅有空白，返回 stripped（可能为空）
            return stripped;
        }
        if line_end.is_none() {
            break;
        }
        search_pos += line_len;
    }
    content
}

/// 组合渲染、标题提取与 wikilink 提取，返回 [`MarkdownDoc`]。
#[must_use]
pub fn render_markdown_doc(content: &str) -> MarkdownDoc {
    let body = strip_frontmatter(content);
    let html = render_html(content);
    let headings = extract_headings(content);
    let wikilinks = crate::link::extract_wikilinks(body);
    MarkdownDoc {
        html,
        wikilinks,
        headings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_simple_markdown() {
        let html = render_html("# Hello\n\nworld");
        assert!(html.contains("<h1>"), "应包含 h1: {html}");
        assert!(html.contains("Hello"));
        assert!(html.contains("<p>world</p>"));
    }

    #[test]
    fn render_wikilink_as_wikilink_href() {
        let html = render_html("[[a|b]]");
        assert!(
            html.contains("href=\"wikilink:a\""),
            "wikilink 应渲染为 wikilink: 前缀: {html}"
        );
        assert!(html.contains(">b</a>"), "显示名应为管道后: {html}");
        let html2 = render_html("[[target]]");
        assert!(html2.contains("href=\"wikilink:target\""), "{html2}");
    }

    #[test]
    fn render_preserves_code_block() {
        let html = render_html("```\n[[a]]\n```");
        assert!(html.contains("<pre>") || html.contains("<code>"), "{html}");
        assert!(
            html.contains("[[a]]"),
            "代码块内 wikilink 应原样保留: {html}"
        );
        assert!(
            !html.contains("wikilink:a"),
            "代码块内不应渲染为链接: {html}"
        );
    }

    #[test]
    fn extract_multiple_headings() {
        let hs = extract_headings("# H1\n\n## H2\n\n### H3");
        assert_eq!(hs, vec!["H1", "H2", "H3"]);
    }

    #[test]
    fn extract_no_headings() {
        let hs = extract_headings("just text\n\n- list");
        assert!(hs.is_empty());
    }

    #[test]
    fn extract_skips_heading_in_code_block() {
        let hs = extract_headings("```\n# not a heading\n```\n\n# real");
        assert_eq!(hs, vec!["real"]);
    }

    #[test]
    fn extract_skips_frontmatter_heading_like() {
        let hs = extract_headings("---\ntitle: hello\n---\n\n# real");
        assert_eq!(hs, vec!["real"]);
    }

    #[test]
    fn strip_with_frontmatter() {
        let content = "---\ntitle: hello\n---\n\n正文";
        assert_eq!(strip_frontmatter(content), "正文");
        let content2 = "---\ntitle: hello\n---\nbody";
        assert_eq!(strip_frontmatter(content2), "body");
    }

    #[test]
    fn strip_without_frontmatter() {
        let content = "# hello\n\nbody";
        assert_eq!(strip_frontmatter(content), content);
    }

    #[test]
    fn strip_empty() {
        assert_eq!(strip_frontmatter(""), "");
    }

    #[test]
    fn strip_unclosed_frontmatter_is_noop() {
        let content = "---\ntitle: hello\n\nbody";
        assert_eq!(strip_frontmatter(content), content);
    }

    #[test]
    fn render_markdown_doc_combines() {
        let doc = render_markdown_doc("---\ntitle: t\n---\n\n# H\n\n[[a|b]]");
        assert!(
            doc.html.contains("wikilink:a"),
            "html 应含 wikilink: {}",
            doc.html
        );
        assert_eq!(doc.headings, vec!["H"]);
        assert_eq!(doc.wikilinks.len(), 1);
        assert_eq!(doc.wikilinks[0].target, "a");
    }

    #[test]
    fn analyze_markdown_matches_separate_paths() {
        let content = "# H1\n\ntext `code [[a]]` more\n\n```\n[[b]]\n```\n\n## H2\n\n[[c]]";
        let analysis = analyze_markdown(content);
        // 标题与独立 `extract_headings` 一致
        assert_eq!(analysis.headings, extract_headings(content));
        assert_eq!(analysis.headings, vec!["H1", "H2"]);
        // 代码块区间非空（行内码 + 围栏块），且 wikilink 提取跳过它们
        assert!(!analysis.code_ranges.is_empty());
        let links = crate::link::extract_wikilinks(content);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "c");
    }

    #[test]
    fn analyze_markdown_empty_and_plain() {
        let empty = analyze_markdown("");
        assert!(empty.headings.is_empty());
        assert!(empty.code_ranges.is_empty());
        let plain = analyze_markdown("just text\n\n- list");
        assert!(plain.headings.is_empty());
        assert!(plain.code_ranges.is_empty());
    }
}
