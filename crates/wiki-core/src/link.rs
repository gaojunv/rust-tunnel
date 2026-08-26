//! `[[...]]` wiki 链接的解析与提取。
//!
//! 当前为骨架：仅提供类型定义与可编译桩，真实解析（`[[target|alias#anchor]]`、
//! `![[...]]` 嵌入语法）在后续批次实现。

use crate::note::NoteKey;

/// 笔记内的一个 wiki 链接。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WikiLink {
    /// 链接目标（笔记相对路径，去扩展名）。
    pub target: String,
    /// 显示别名（`[[target|alias]]` 的 `alias` 段）。
    pub alias: Option<String>,
    /// 页内锚点（`[[target#anchor]]` 的 `anchor` 段）。
    pub anchor: Option<String>,
    /// 是否为嵌入语法（`![[...]]`）。
    pub embed: bool,
}

/// 链接目标的消歧结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedLink {
    /// 唯一命中一个笔记。
    Resolved(NoteKey),
    /// 多个候选笔记（同名/别名冲突）。
    Ambiguous(Vec<NoteKey>),
    /// 目标不存在（保留原始 target 字符串）。
    Broken(String),
}

/// 解析单个 `[[...]]` 或 `![[...]]` 链接。
///
/// 当前为桩实现：恒返回 `None`。
#[must_use]
pub fn parse_wikilink(_text: &str) -> Option<WikiLink> {
    None
}

/// 从正文中提取全部 wiki 链接。
///
/// 当前为桩实现：恒返回空列表。
#[must_use]
pub fn extract_wikilinks(_content: &str) -> Vec<WikiLink> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_wikilink_stub_returns_none() {
        assert_eq!(parse_wikilink("[[a|b#c]]"), None);
        assert_eq!(parse_wikilink("![[img.png]]"), None);
    }

    #[test]
    fn extract_stub_returns_empty() {
        assert!(extract_wikilinks("[[a]] and [[b]]").is_empty());
    }
}
