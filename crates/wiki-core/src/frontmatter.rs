//! 笔记 frontmatter（YAML 前置元数据）的解析与提取。
//!
//! 使用 [`gray_matter`] 解析 YAML frontmatter，提取已知字段到 [`FrontMatter`]，
//! 其余键值保留在 `extra` 中。定界符定位对齐 comrak 行为（含分隔符本身）。

use gray_matter::engine::YAML;
use gray_matter::Matter;
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

/// 将 frontmatter 中的字符串或字符串数组提取为 `Vec<String>`。
fn extract_string_vec(value: serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(s) => vec![s],
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// 解析正文头部 frontmatter。
///
/// 使用 [`gray_matter::Matter`]（YAML 引擎）解析，提取 `ref`、`title`、
/// `aliases`、`tags` 字段，其余键值作为 `extra: serde_json::Value::Object` 保留。
/// 解析失败或无 frontmatter 时返回 `extra` 为 `Null` 的空结构，不 panic。
#[must_use]
pub fn parse_frontmatter(text: &str) -> FrontMatter {
    let matter = Matter::<YAML>::new();
    let parsed = matter.parse::<serde_json::Value>(text);
    match parsed {
        Ok(entity) => match entity.data {
            None => FrontMatter {
                ref_id: None,
                title: None,
                aliases: Vec::new(),
                tags: Vec::new(),
                extra: serde_json::Value::Null,
            },
            Some(value) => {
                if let serde_json::Value::Object(mut map) = value {
                    let ref_id = map.remove("ref").and_then(|v| match v {
                        serde_json::Value::String(s) => RefId::parse(&s),
                        _ => None,
                    });
                    let title = map.remove("title").and_then(|v| match v {
                        serde_json::Value::String(s) => Some(s),
                        _ => None,
                    });
                    let aliases = map
                        .remove("aliases")
                        .map(extract_string_vec)
                        .unwrap_or_default();
                    let tags = map
                        .remove("tags")
                        .map(extract_string_vec)
                        .unwrap_or_default();
                    // `alias` 单数形式兼容（Obsidian 既支持 `aliases` 也支持 `alias`）
                    let aliases = if aliases.is_empty() {
                        map.remove("alias")
                            .map(extract_string_vec)
                            .unwrap_or_default()
                    } else {
                        // 已有 `aliases` 时仍需清理残留的 `alias` 键，避免泄漏到 `extra`
                        map.remove("alias");
                        aliases
                    };
                    let extra = serde_json::Value::Object(map);
                    FrontMatter {
                        ref_id,
                        title,
                        aliases,
                        tags,
                        extra,
                    }
                } else {
                    FrontMatter {
                        ref_id: None,
                        title: None,
                        aliases: Vec::new(),
                        tags: Vec::new(),
                        extra: serde_json::Value::Null,
                    }
                }
            }
        },
        Err(_) => FrontMatter {
            ref_id: None,
            title: None,
            aliases: Vec::new(),
            tags: Vec::new(),
            extra: serde_json::Value::Null,
        },
    }
}

/// 定位 frontmatter 定界符 `---` 在原文中的字节区间 `(start, end)`。
///
/// 检查 `text` 是否以 `---\n` 开头，找到第二个 `---\n` 或文件末尾的 `---`，
/// 返回含分隔符的字节区间 `[start, end)`，对齐 comrak 的 `FrontMatter(s)`
///（其内容包含首尾 `---` 分隔符本身）。无 frontmatter 时返回 `None`。
#[must_use]
pub fn extract_frontmatter_delimiter(text: &str) -> Option<(usize, usize)> {
    const OPENER: &str = "---\n";
    if !text.starts_with(OPENER) {
        return None;
    }
    let opener_len = OPENER.len();
    let rest = &text[opener_len..];

    // 紧邻的空 frontmatter：`---\n---\n` 或 `---\n---`
    if rest.starts_with("---\n") {
        let mut end = opener_len + "---\n".len();
        if text[end..].starts_with('\n') {
            end += 1;
        } else if text[end..].starts_with("\r\n") {
            end += 2;
        }
        return Some((0, end));
    }
    if rest == "---" {
        return Some((0, text.len()));
    }

    // 形如 `\n---\n` 的关闭分隔符（可能后接一个空行，comrak 会将其一并纳入）
    if let Some(pos) = rest.find("\n---\n") {
        let mut end = opener_len + pos + "\n---\n".len();
        if text[end..].starts_with('\n') {
            end += 1;
        } else if text[end..].starts_with("\r\n") {
            end += 2;
        }
        return Some((0, end));
    }

    // 关闭分隔符位于文件末尾且无尾随换行：`\n---` 结尾
    if rest.ends_with("\n---") {
        return Some((0, text.len()));
    }

    // 也支持 `\r\n---\r\n` / `\r\n---` 的 CRLF 变体
    if let Some(pos) = rest.find("\r\n---\r\n") {
        let mut end = opener_len + pos + "\r\n---\r\n".len();
        if text[end..].starts_with("\r\n") {
            end += 2;
        } else if text[end..].starts_with('\n') {
            end += 1;
        }
        return Some((0, end));
    }
    if rest.ends_with("\r\n---") {
        return Some((0, text.len()));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_standard_frontmatter() {
        let text = "---\nref: deploy/prod\ntitle: Hello\naliases:\n  - Alias1\n  - alias2\ntags:\n  - tag1\n  - tag2\n---\nbody";
        let fm = parse_frontmatter(text);
        assert_eq!(fm.ref_id.as_ref().map(RefId::as_str), Some("deploy/prod"));
        assert_eq!(fm.title.as_deref(), Some("Hello"));
        assert_eq!(fm.aliases, vec!["Alias1", "alias2"]);
        assert_eq!(fm.tags, vec!["tag1", "tag2"]);
        // 无额外字段时 extra 为空对象
        assert!(fm.extra.is_object());
        assert_eq!(fm.extra.as_object().map(serde_json::Map::len), Some(0));

        // 额外字段应保留在 extra
        let text3 =
            "---\nref: a/b\ntitle: T\naliases: [x]\ntags: [y]\ncustom: 123\nextra_field: value\n---\nbody";
        let fm3 = parse_frontmatter(text3);
        assert!(fm3.extra.is_object());
        let obj = fm3.extra.as_object().expect("extra 应为对象");
        assert_eq!(
            obj.get("custom").and_then(serde_json::Value::as_i64),
            Some(123)
        );
        assert_eq!(
            obj.get("extra_field").and_then(serde_json::Value::as_str),
            Some("value")
        );
        // 已提取字段不应残留在 extra
        assert!(!obj.contains_key("ref"));
        assert!(!obj.contains_key("title"));
    }

    #[test]
    fn parse_frontmatter_string_aliases_and_tags() {
        let text = "---\ntitle: T\naliases: single\ntags: single-tag\n---\nbody";
        let fm = parse_frontmatter(text);
        assert_eq!(fm.aliases, vec!["single"]);
        assert_eq!(fm.tags, vec!["single-tag"]);
    }

    #[test]
    fn parse_no_frontmatter() {
        let fm = parse_frontmatter("plain body without frontmatter");
        assert!(fm.ref_id.is_none());
        assert!(fm.title.is_none());
        assert!(fm.aliases.is_empty());
        assert!(fm.tags.is_empty());
        assert_eq!(fm.extra, serde_json::Value::Null);
        assert_eq!(extract_frontmatter_delimiter("plain body"), None);
    }

    #[test]
    fn parse_empty_frontmatter() {
        let fm = parse_frontmatter("---\n---\nbody");
        assert!(fm.ref_id.is_none());
        assert!(fm.title.is_none());
        assert!(fm.aliases.is_empty());
        assert!(fm.tags.is_empty());
        // 空 frontmatter 视为无数据，extra 为 Null（gray_matter 对空 matter 返回 None）
        assert_eq!(fm.extra, serde_json::Value::Null);

        // 另一形式：无 body 的空 frontmatter
        let fm2 = parse_frontmatter("---\n---\n");
        assert_eq!(fm2.extra, serde_json::Value::Null);

        // 定界符应能定位
        assert_eq!(
            extract_frontmatter_delimiter("---\n---\nbody"),
            Some((0, 8))
        );
        assert_eq!(extract_frontmatter_delimiter("---\n---\n"), Some((0, 8)));
        assert_eq!(extract_frontmatter_delimiter("---\n---"), Some((0, 7)));
    }

    #[test]
    fn parse_illegal_yaml_returns_null_extra() {
        let fm = parse_frontmatter("---\n: bad: : yaml: :\n---\nbody");
        // 非法 YAML 应不 panic，且 extra 为 Null
        assert!(fm.ref_id.is_none());
        assert!(fm.title.is_none());
        assert_eq!(fm.extra, serde_json::Value::Null);
        // 但定界符仍应被识别
        assert!(extract_frontmatter_delimiter("---\n: bad: : yaml: :\n---\nbody").is_some());
    }

    #[test]
    fn extra_fields_retained() {
        let text = "---\ntitle: T\ncustom_key: hello\nnested:\n  a: 1\n  b: 2\n---\nbody";
        let fm = parse_frontmatter(text);
        assert_eq!(fm.title.as_deref(), Some("T"));
        let obj = fm.extra.as_object().expect("extra 应为对象");
        assert_eq!(
            obj.get("custom_key").and_then(serde_json::Value::as_str),
            Some("hello")
        );
        assert!(obj.get("nested").is_some());
        assert!(!obj.contains_key("title"));
    }

    #[test]
    fn delimiter_standard_and_with_blank_line() {
        // 标准：关闭分隔符后无空行
        let text = "---\ntitle: hello\n---\nbody";
        let delim = extract_frontmatter_delimiter(text).expect("应找到定界符");
        assert_eq!(delim.0, 0);
        assert_eq!(&text[delim.0..delim.1], "---\ntitle: hello\n---\n");
        assert_eq!(&text[delim.1..], "body");

        // 关闭分隔符后带一个空行（comrak 会将其纳入 FrontMatter）
        let text2 = "---\ntitle: hello\n---\n\nbody";
        let delim2 = extract_frontmatter_delimiter(text2).expect("应找到定界符");
        assert_eq!(&text2[delim2.0..delim2.1], "---\ntitle: hello\n---\n\n");
        assert_eq!(&text2[delim2.1..], "body");
    }

    #[test]
    fn delimiter_no_frontmatter() {
        assert_eq!(extract_frontmatter_delimiter("body without delim"), None);
        assert_eq!(extract_frontmatter_delimiter(" ---\ntitle: a\n---\n"), None);
        assert_eq!(extract_frontmatter_delimiter(""), None);
    }

    #[test]
    fn delimiter_at_eof_without_trailing_newline() {
        let text = "---\ntitle: hello\n---";
        let delim = extract_frontmatter_delimiter(text).expect("末尾无换行也应识别");
        assert_eq!(delim, (0, text.len()));
    }

    #[test]
    fn ref_parsing_invalid_is_none() {
        let text = "---\nref: \"!!! invalid ref !!!\"\n---\nbody";
        let fm = parse_frontmatter(text);
        assert!(fm.ref_id.is_none());
        // 非法 ref 不应污染 extra（已被移除，校验失败则丢弃）
        assert!(!fm.extra.as_object().is_some_and(|m| m.contains_key("ref")));
    }
}
