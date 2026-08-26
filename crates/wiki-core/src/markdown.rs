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

/// 提取所有标题的文本（跳过 frontmatter）。
#[must_use]
pub fn extract_headings(content: &str) -> Vec<String> {
    use comrak::nodes::NodeValue;
    use comrak::{parse_document, Arena};

    let options = comrak_options();
    let arena = Arena::new();
    let root = parse_document(&arena, content, &options);
    let mut headings = Vec::new();
    for node in root.descendants() {
        let ast = node.data();
        if !matches!(ast.value, NodeValue::Heading(_)) {
            continue;
        }
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
    headings
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
}
