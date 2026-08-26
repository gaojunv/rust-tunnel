//! Markdown 渲染与结构提取。
//!
//! 当前为骨架：仅提供类型定义与可编译桩；HTML 渲染（comrak）、标题提取、
//! frontmatter 剥离在后续批次实现。

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

/// 用 comrak 将 Markdown 渲染为 HTML。
///
/// 当前为桩实现：恒返回空字符串。
#[must_use]
pub fn render_html(_content: &str) -> String {
    String::new()
}

/// 提取所有标题的文本。
///
/// 当前为桩实现：恒返回空列表。
#[must_use]
pub fn extract_headings(_content: &str) -> Vec<String> {
    Vec::new()
}

/// 去掉 frontmatter 后返回正文部分。
///
/// 当前为桩实现：原样返回 `content`。
#[must_use]
pub fn strip_frontmatter(content: &str) -> &str {
    content
}
