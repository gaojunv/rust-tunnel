//! `[[...]]` wiki 链接的解析与提取。
//!
//! 不依赖 comrak 的 wikilink 节点——comrak 0.54 只支持 `[[target|alias]]`
//! 的 `UrlFirst` 形态，且不支持 `#anchor` 拆分与 `![[...]]` 嵌入；
//! 本模块用手动扫描实现完整语法，并用 comrak 仅收集代码块范围以跳过
//! 代码块内的误匹配。

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
/// 支持 `[[target]]`、`[[target|alias]]`、`[[target#anchor]]`、
/// `[[target|alias#anchor]]` 与嵌入 `![[target]]`。管道前的 `target` 为
/// 必填，空或全空白视为非法。`alias` 与 `anchor` 若出现也必须非空。
#[must_use]
pub fn parse_wikilink(text: &str) -> Option<WikiLink> {
    let trimmed = text.trim();
    let (inner, embed) = if trimmed.starts_with("![[") && trimmed.ends_with("]]") {
        if trimmed.len() < 5 {
            return None;
        }
        (&trimmed[3..trimmed.len() - 2], true)
    } else if trimmed.starts_with("[[") && trimmed.ends_with("]]") {
        if trimmed.len() < 4 {
            return None;
        }
        (&trimmed[2..trimmed.len() - 2], false)
    } else {
        return None;
    };
    let inner_trimmed = inner.trim();
    if inner_trimmed.is_empty() {
        return None;
    }
    if let Some(pipe_idx) = inner_trimmed.find('|') {
        let target_raw = &inner_trimmed[..pipe_idx];
        let rest = &inner_trimmed[pipe_idx + 1..];
        let target = target_raw.trim();
        if target.is_empty() || rest.trim().is_empty() {
            return None;
        }
        if let Some(hash_idx) = rest.find('#') {
            let alias_raw = &rest[..hash_idx];
            let anchor_raw = &rest[hash_idx + 1..];
            let alias = alias_raw.trim();
            let anchor = anchor_raw.trim();
            if alias.is_empty() || anchor.is_empty() {
                return None;
            }
            Some(WikiLink {
                target: target.to_owned(),
                alias: Some(alias.to_owned()),
                anchor: Some(anchor.to_owned()),
                embed,
            })
        } else {
            let alias = rest.trim();
            Some(WikiLink {
                target: target.to_owned(),
                alias: Some(alias.to_owned()),
                anchor: None,
                embed,
            })
        }
    } else if let Some(hash_idx) = inner_trimmed.find('#') {
        let target_raw = &inner_trimmed[..hash_idx];
        let anchor_raw = &inner_trimmed[hash_idx + 1..];
        let target = target_raw.trim();
        let anchor = anchor_raw.trim();
        if target.is_empty() || anchor.is_empty() {
            return None;
        }
        Some(WikiLink {
            target: target.to_owned(),
            alias: None,
            anchor: Some(anchor.to_owned()),
            embed,
        })
    } else {
        Some(WikiLink {
            target: inner_trimmed.to_owned(),
            alias: None,
            anchor: None,
            embed,
        })
    }
}

/// 内部扫描器：返回带字节区间的 wiki 链接（`start` 含 `!` 前缀，`end` 为开区间）。
fn scan_wikilinks_with_spans(content: &str) -> Vec<(WikiLink, usize, usize)> {
    let code_ranges = collect_code_ranges(content);
    let mut out = Vec::new();
    let mut pos = 0usize;
    let len = content.len();
    while pos < len {
        let remaining = &content[pos..];
        let Some(rel) = remaining.find("[[") else {
            break;
        };
        let abs_pos = pos + rel;
        let candidate_start = if abs_pos > 0 && content.as_bytes().get(abs_pos - 1) == Some(&b'!') {
            abs_pos - 1
        } else {
            abs_pos
        };
        if is_in_code_range(candidate_start, &code_ranges)
            || is_in_code_range(abs_pos, &code_ranges)
        {
            pos = abs_pos + 2;
            continue;
        }
        let after_open = abs_pos + 2;
        if after_open > len {
            break;
        }
        let search = &content[after_open..];
        let Some(close_rel) = search.find("]]") else {
            break;
        };
        let close_pos = after_open + close_rel;
        let candidate_end = close_pos + 2;
        // 若 opening 与首个 closing 之间还包含另一个 `[[`，视为嵌套，跳过外层以捕获最短闭合的内层
        let inner_slice = &content[abs_pos + 2..close_pos];
        if inner_slice.contains("[[") {
            pos = abs_pos + 2;
            if let Some(inner_rel) = content[abs_pos + 2..close_pos].find("[[") {
                pos = abs_pos + 2 + inner_rel;
            }
            continue;
        }
        if ranges_overlap(candidate_start, candidate_end, &code_ranges) {
            pos = candidate_end;
            continue;
        }
        let candidate = &content[candidate_start..candidate_end];
        if let Some(link) = parse_wikilink(candidate) {
            out.push((link, candidate_start, candidate_end));
        }
        pos = candidate_end;
    }
    out
}

/// 从正文中提取全部 wiki 链接。
///
/// 手动扫描 `[[` / `![[` 并以最短闭合 `]]` 配对；落在代码块
/// （`CodeBlock` / `Code`）内的位置通过 comrak `sourcepos` 换算字节
/// 区间后跳过。内部复用带区间的扫描器，对外丢弃区间以保持签名不变。
#[must_use]
pub fn extract_wikilinks(content: &str) -> Vec<WikiLink> {
    scan_wikilinks_with_spans(content)
        .into_iter()
        .map(|(link, _, _)| link)
        .collect()
}

/// 按 `rename` 映射重写 `content` 中的 wikilink `target`，返回 `(新内容, 替换次数)`。
///
/// - 只替换链接的 `target` 部分，保留 `alias`（`|alias`）、`anchor`（`#anchor`）、
///   `embed`（`![[ ]]`）形态；跳过代码块/行内码（复用现有 `code-range` 机制）。
/// - 匹配语义与 [`resolve_link`] 对齐：`rename` 的 key 统一小写比较；先全路径
///   精确匹配，再按 `basename`（最后一段）匹配，但仅当该 `basename` 在 `rename`
///   key 集合中唯一时才改（歧义不改）。
/// - 从后往前按 `span` 替换，避免偏移失效。
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn rewrite_wikilinks(
    content: &str,
    rename: &std::collections::HashMap<String, String>,
) -> (String, usize) {
    if rename.is_empty() {
        return (content.to_owned(), 0);
    }
    // 统一小写：保持值的原始大小写
    let mut lower_map: std::collections::HashMap<String, String> =
        std::collections::HashMap::with_capacity(rename.len());
    for (k, v) in rename {
        lower_map.insert(k.to_lowercase(), v.clone());
    }
    // 统计 basename 出现次数，用于歧义判断
    let mut basename_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for k in lower_map.keys() {
        let base = k.rsplit('/').next().unwrap_or(k.as_str()).to_owned();
        *basename_counts.entry(base).or_insert(0) += 1;
    }
    let mut basename_unique: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for (k, v) in &lower_map {
        let base = k.rsplit('/').next().unwrap_or(k.as_str()).to_owned();
        if basename_counts.get(&base).copied().unwrap_or(0) == 1 {
            basename_unique.insert(base, v.clone());
        }
    }

    let spans = scan_wikilinks_with_spans(content);
    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    for (link, start, end) in spans {
        let target_lower = link.target.to_lowercase();
        let new_target_opt = if let Some(v) = lower_map.get(&target_lower) {
            Some(v.clone())
        } else {
            let base = target_lower
                .rsplit('/')
                .next()
                .unwrap_or(target_lower.as_str())
                .to_owned();
            basename_unique.get(&base).cloned()
        };
        let Some(new_target) = new_target_opt else {
            continue;
        };
        if new_target == link.target {
            continue;
        }
        let prefix = if link.embed { "![[" } else { "[[" };
        let alias_part = link
            .alias
            .as_ref()
            .map(|a| format!("|{a}"))
            .unwrap_or_default();
        let anchor_part = link
            .anchor
            .as_ref()
            .map(|a| format!("#{a}"))
            .unwrap_or_default();
        let new_link = format!("{prefix}{new_target}{alias_part}{anchor_part}]]");
        replacements.push((start, end, new_link));
    }
    if replacements.is_empty() {
        return (content.to_owned(), 0);
    }
    let mut result = content.to_owned();
    // 从后往前替换，避免偏移失效
    for (start, end, new_text) in replacements.iter().rev() {
        result.replace_range(*start..*end, new_text);
    }
    let count = replacements.len();
    (result, count)
}

/// 将 `WikiLink` 解析到笔记集合。
///
/// 三级匹配（大小写不敏感）：
/// 1. 完整路径相等（`target` 与 `NoteKey` 全串相等）→ 唯一则 `Resolved`，多条则 `Ambiguous`
/// 2. 否则按 `basename`（`/` 后最后一段）匹配 → 唯一则 `Resolved`，多条则 `Ambiguous`
/// 3. 均无命中 → `Broken(target)`
#[must_use]
pub fn resolve_link(link: &WikiLink, all_notes: &[NoteKey]) -> ResolvedLink {
    let target_lower = link.target.to_lowercase();
    let full_matches: Vec<NoteKey> = all_notes
        .iter()
        .filter(|k| k.as_str().to_lowercase() == target_lower)
        .cloned()
        .collect();
    if full_matches.len() == 1 {
        if let Some(first) = full_matches.into_iter().next() {
            return ResolvedLink::Resolved(first);
        }
    } else if full_matches.len() > 1 {
        return ResolvedLink::Ambiguous(full_matches);
    }
    let target_basename = target_lower.rsplit('/').next().unwrap_or(&target_lower);
    let basename_matches: Vec<NoteKey> = all_notes
        .iter()
        .filter(|k| {
            let base = k
                .as_str()
                .rsplit('/')
                .next()
                .unwrap_or(k.as_str())
                .to_lowercase();
            base == target_basename
        })
        .cloned()
        .collect();
    if basename_matches.len() == 1 {
        if let Some(first) = basename_matches.into_iter().next() {
            return ResolvedLink::Resolved(first);
        }
    } else if basename_matches.len() > 1 {
        return ResolvedLink::Ambiguous(basename_matches);
    }
    ResolvedLink::Broken(link.target.clone())
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

fn collect_code_ranges(content: &str) -> Vec<(usize, usize)> {
    use comrak::nodes::NodeValue;
    use comrak::{Arena, Options};
    let mut options = Options::default();
    options.extension.front_matter_delimiter = Some("---".to_owned());
    let arena = Arena::new();
    let root = comrak::parse_document(&arena, content, &options);
    let line_starts = compute_line_starts(content);
    let content_len = content.len();
    let mut ranges = Vec::new();
    for node in root.descendants() {
        let ast = node.data();
        let is_code = matches!(ast.value, NodeValue::CodeBlock(_) | NodeValue::Code(_));
        if !is_code {
            continue;
        }
        let sp = ast.sourcepos;
        if sp.start.line == 0 || sp.end.line == 0 {
            continue;
        }
        let Some(start) = byte_offset(&line_starts, content_len, sp.start.line, sp.start.column)
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
            ranges.push((start, end_clamped));
        }
    }
    ranges
}

fn is_in_code_range(pos: usize, ranges: &[(usize, usize)]) -> bool {
    ranges.iter().any(|(s, e)| pos >= *s && pos < *e)
}

fn ranges_overlap(start: usize, end: usize, ranges: &[(usize, usize)]) -> bool {
    ranges.iter().any(|(s, e)| start < *e && end > *s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_target() {
        assert_eq!(
            parse_wikilink("[[target]]"),
            Some(WikiLink {
                target: "target".to_owned(),
                alias: None,
                anchor: None,
                embed: false
            })
        );
    }

    #[test]
    fn parse_with_alias() {
        assert_eq!(
            parse_wikilink("[[a|b]]"),
            Some(WikiLink {
                target: "a".to_owned(),
                alias: Some("b".to_owned()),
                anchor: None,
                embed: false
            })
        );
    }

    #[test]
    fn parse_with_anchor() {
        assert_eq!(
            parse_wikilink("[[a#h]]"),
            Some(WikiLink {
                target: "a".to_owned(),
                alias: None,
                anchor: Some("h".to_owned()),
                embed: false
            })
        );
    }

    #[test]
    fn parse_with_alias_and_anchor() {
        assert_eq!(
            parse_wikilink("[[a|b#h]]"),
            Some(WikiLink {
                target: "a".to_owned(),
                alias: Some("b".to_owned()),
                anchor: Some("h".to_owned()),
                embed: false
            })
        );
    }

    #[test]
    fn parse_embed() {
        assert_eq!(
            parse_wikilink("![[img.png]]"),
            Some(WikiLink {
                target: "img.png".to_owned(),
                alias: None,
                anchor: None,
                embed: true
            })
        );
        assert_eq!(
            parse_wikilink("![[a|b#c]]"),
            Some(WikiLink {
                target: "a".to_owned(),
                alias: Some("b".to_owned()),
                anchor: Some("c".to_owned()),
                embed: true
            })
        );
    }

    #[test]
    fn parse_empty_is_none() {
        assert_eq!(parse_wikilink("[[ ]]"), None);
        assert_eq!(parse_wikilink("[[]]"), None);
        assert_eq!(parse_wikilink("[[|b]]"), None);
        assert_eq!(parse_wikilink("[[a|]]"), None);
        assert_eq!(parse_wikilink("![[ ]]"), None);
    }

    #[test]
    fn parse_trims_whitespace() {
        assert_eq!(
            parse_wikilink("  [[ a | b # h ]]  "),
            Some(WikiLink {
                target: "a".to_owned(),
                alias: Some("b".to_owned()),
                anchor: Some("h".to_owned()),
                embed: false
            })
        );
    }

    #[test]
    fn extract_multiple_links() {
        let links = extract_wikilinks("[[a]] and [[b]]");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].target, "a");
        assert_eq!(links[1].target, "b");
    }

    #[test]
    fn extract_mixed_embed_and_normal() {
        let links = extract_wikilinks("![[embed]] and [[a]]");
        assert_eq!(links.len(), 2);
        assert!(links[0].embed);
        assert_eq!(links[0].target, "embed");
        assert!(!links[1].embed);
        assert_eq!(links[1].target, "a");
    }

    #[test]
    fn extract_skips_code_block() {
        let content = "```\n[[inside]]\n```\n[[outside]]";
        let links = extract_wikilinks(content);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "outside");
    }

    #[test]
    fn extract_skips_inline_code() {
        let content = "hello `[[inside]]` and [[outside]]";
        let links = extract_wikilinks(content);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "outside");
    }

    #[test]
    fn extract_nested_shortest_closure() {
        // 内层 `[[b]]` 是最短闭合，应被捕获
        let links = extract_wikilinks("[[a[[b]]]]");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "b");
    }

    #[test]
    fn resolve_full_path() {
        let notes = vec![
            NoteKey::new("a/b".to_owned()),
            NoteKey::new("c/d".to_owned()),
        ];
        let link = WikiLink {
            target: "a/b".to_owned(),
            alias: None,
            anchor: None,
            embed: false,
        };
        assert_eq!(
            resolve_link(&link, &notes),
            ResolvedLink::Resolved(NoteKey::new("a/b".to_owned()))
        );
    }

    #[test]
    fn resolve_case_insensitive() {
        let notes = vec![NoteKey::new("a/B".to_owned())];
        let link = WikiLink {
            target: "A/b".to_owned(),
            alias: None,
            anchor: None,
            embed: false,
        };
        assert_eq!(
            resolve_link(&link, &notes),
            ResolvedLink::Resolved(NoteKey::new("a/B".to_owned()))
        );
    }

    #[test]
    fn resolve_basename_unique() {
        let notes = vec![
            NoteKey::new("a/b".to_owned()),
            NoteKey::new("c/d".to_owned()),
        ];
        let link = WikiLink {
            target: "b".to_owned(),
            alias: None,
            anchor: None,
            embed: false,
        };
        assert_eq!(
            resolve_link(&link, &notes),
            ResolvedLink::Resolved(NoteKey::new("a/b".to_owned()))
        );
    }

    #[test]
    fn resolve_basename_ambiguous() {
        let notes = vec![
            NoteKey::new("a/b".to_owned()),
            NoteKey::new("c/b".to_owned()),
        ];
        let link = WikiLink {
            target: "b".to_owned(),
            alias: None,
            anchor: None,
            embed: false,
        };
        match resolve_link(&link, &notes) {
            ResolvedLink::Ambiguous(v) => assert_eq!(v.len(), 2),
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn resolve_broken() {
        let notes = vec![NoteKey::new("a/b".to_owned())];
        let link = WikiLink {
            target: "missing".to_owned(),
            alias: None,
            anchor: None,
            embed: false,
        };
        assert_eq!(
            resolve_link(&link, &notes),
            ResolvedLink::Broken("missing".to_owned())
        );
    }

    #[test]
    fn rewrite_preserves_alias_anchor_embed() {
        let mut map = std::collections::HashMap::new();
        map.insert("a".to_owned(), "b".to_owned());
        let (out, n) = rewrite_wikilinks("[[a|alias#sec]] and ![[a]]", &map);
        assert_eq!(n, 2);
        assert!(out.contains("[[b|alias#sec]]"), "alias/anchor 保留：{out}");
        assert!(out.contains("![[b]]"), "embed 保留：{out}");
    }

    #[test]
    fn rewrite_skips_code_span_and_fence() {
        let mut map = std::collections::HashMap::new();
        map.insert("a".to_owned(), "b".to_owned());
        let content = "```\n[[a]]\n```\n[[a]] and `[[a]]` then [[a]]";
        let (out, n) = rewrite_wikilinks(content, &map);
        assert_eq!(n, 2);
        assert!(out.contains("```\n[[a]]\n```"), "fence 内不改");
        assert!(out.contains("`[[a]]`"), "行内码不改");
        // 非代码区已改
        let outside_count = out.matches("[[b]]").count();
        assert_eq!(outside_count, 2, "非代码区应改 2 处：{out}");
    }

    #[test]
    fn rewrite_case_insensitive() {
        let mut map = std::collections::HashMap::new();
        map.insert("A/B".to_owned(), "x/y".to_owned());
        let (out, n) = rewrite_wikilinks("[[a/b]] and [[A/b]]", &map);
        assert_eq!(n, 2);
        assert_eq!(out.matches("[[x/y]]").count(), 2);
    }

    #[test]
    fn rewrite_basename_ambiguous_not_rewritten() {
        let mut map = std::collections::HashMap::new();
        map.insert("a/note".to_owned(), "a/new-a".to_owned());
        map.insert("b/note".to_owned(), "b/new-b".to_owned());
        let (out, n) = rewrite_wikilinks("[[note]]", &map);
        assert_eq!(n, 0, "basename 歧义不改");
        assert_eq!(out, "[[note]]");
    }

    #[test]
    fn rewrite_basename_unique_rewritten() {
        let mut map = std::collections::HashMap::new();
        map.insert("a/note".to_owned(), "a/new-a".to_owned());
        let (out, n) = rewrite_wikilinks("[[note]]", &map);
        assert_eq!(n, 1);
        assert!(out.contains("[[a/new-a]]"));
    }

    #[test]
    fn rewrite_no_match_unchanged() {
        let mut map = std::collections::HashMap::new();
        map.insert("x".to_owned(), "y".to_owned());
        let content = "[[a]] and [[b]]";
        let (out, n) = rewrite_wikilinks(content, &map);
        assert_eq!(n, 0);
        assert_eq!(out, content);
    }

    #[test]
    fn rewrite_nested_and_adjacent() {
        let mut map = std::collections::HashMap::new();
        map.insert("a".to_owned(), "x".to_owned());
        map.insert("b".to_owned(), "y".to_owned());
        let (out, n) = rewrite_wikilinks("[[a]][[b]] and [[a]] [[b]]", &map);
        assert_eq!(n, 4);
        assert!(out.contains("[[x]][[y]]"));
        assert!(out.contains("[[x]] [[y]]"));
    }

    #[test]
    fn rewrite_empty_map_noop() {
        let map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let content = "[[a]]";
        let (out, n) = rewrite_wikilinks(content, &map);
        assert_eq!(n, 0);
        assert_eq!(out, content);
    }
}
