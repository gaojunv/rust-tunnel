//! 笔记 frontmatter（YAML 前置元数据）的解析与提取。
//!
//! 当前为骨架：仅提供类型定义与可编译桩，真正的 YAML 解析（基于
//! `gray_matter`）与定界符定位在后续批次实现。

use serde::{Deserialize, Serialize};

use crate::ref_id::RefId;

/// 从笔记正文头部解析出的前置元数据。
///
/// `extra` 保留所有未识别的键值，供前端原样展示或后续版本消费。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontMatter {
    /// 显式声明的 remote `ref`；未声明时可改由路径推导。
    pub ref_id: Option<RefId>,
    /// 显示标题（优先级高于路径推导出的文件名）。
    pub title: Option<String>,
    /// 别名列表（检索/链接解析的候选目标名）。
    pub aliases: Vec<String>,
    /// 标签列表。
    pub tags: Vec<String>,
    /// 未识别的其它 frontmatter 键值。
    pub extra: serde_json::Value,
}

/// 解析正文头部 frontmatter。
///
/// 当前为桩实现：恒返回空 frontmatter（`extra` 为 `Null`），不读取 `text` 内容。
#[must_use]
pub fn parse_frontmatter(_text: &str) -> FrontMatter {
    FrontMatter {
        ref_id: None,
        title: None,
        aliases: Vec::new(),
        tags: Vec::new(),
        extra: serde_json::Value::Null,
    }
}

/// 定位 frontmatter 定界符 `---` 在原文中的字节区间 `(start, end)`。
///
/// 无 frontmatter 时返回 `None`。当前为桩实现：恒返回 `None`。
#[must_use]
pub fn extract_frontmatter_delimiter(_text: &str) -> Option<(usize, usize)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frontmatter_stub_returns_empty_extra() {
        let fm = parse_frontmatter("---\ntitle: A\n---\nbody");
        assert!(fm.ref_id.is_none());
        assert!(fm.title.is_none());
        assert!(fm.aliases.is_empty());
        assert_eq!(fm.extra, serde_json::Value::Null);
    }

    #[test]
    fn delimiter_stub_returns_none() {
        assert_eq!(extract_frontmatter_delimiter("---\ntitle: A\n---"), None);
    }
}
