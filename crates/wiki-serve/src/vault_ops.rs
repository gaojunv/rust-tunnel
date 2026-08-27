// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

/*!
Vault 操作的纯逻辑层（完全不依赖 `tauri`）。

全部函数签名均为 `fn xxx(root: &Path, ...) -> IpcResult<...>`，便于不带
`tauri` feature 时被 `cargo test` 直接覆盖。
*/

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rust_tunnel_wiki_core::graph::LinkGraph;
use rust_tunnel_wiki_core::vault::{Vault, VaultScanner};

use crate::dto::{GraphDto, GraphEdge, GraphNode, NoteDto, NoteSummary, SearchHitDto, VaultInfo};
use crate::error::{IpcError, IpcResult};

// ---------------------------------------------------------------------------
// 路径安全
// ---------------------------------------------------------------------------

/// 将 `key` 解析为 vault 内的笔记文件路径（`root/key.md`），并做安全校验。
///
/// # Errors
///
/// - `key` 为空
/// - `key` 为绝对路径
/// - `key` 含 `..` 段（`/` 或 `\` 分隔）
/// - 归一化后不在 `root` 之内
pub fn resolve_note_path(root: &Path, key: &str) -> IpcResult<PathBuf> {
    if key.is_empty() {
        return Err(IpcError::PathTraversal("空 key".to_owned()));
    }
    if key.trim().is_empty() {
        return Err(IpcError::PathTraversal("空白 key".to_owned()));
    }
    if Path::new(key).is_absolute() {
        return Err(IpcError::PathTraversal(format!("绝对路径：{key}")));
    }
    // Windows 反斜杠归一后检查
    let normalized = key.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(IpcError::PathTraversal(format!("绝对路径：{key}")));
    }
    // 拒绝任何 `..` 段
    if normalized.split('/').any(|seg| seg == "..") {
        return Err(IpcError::PathTraversal(format!("路径逃逸：{key}")));
    }
    // 额外拒绝含 `:` 的 Windows 盘符形态
    if normalized.contains(':') {
        return Err(IpcError::PathTraversal(format!("非法路径：{key}")));
    }

    let candidate = root.join(&normalized).with_extension("md");
    // 归一后必须仍在 root 之内（词法检查）
    if !candidate.starts_with(root) {
        return Err(IpcError::PathTraversal(format!("逃逸到 root 之外：{key}")));
    }
    Ok(candidate)
}

// ---------------------------------------------------------------------------
// frontmatter title 注入
// ---------------------------------------------------------------------------

/// 将 `desired` 注入为正文的 frontmatter `title`。
///
/// 若原文已含相同 `title` 则原样返回；否则重建 frontmatter 块，保留 `ref`、
/// `aliases`、`tags` 与 `extra`，并在首部写入 `title: <desired>`。
fn merge_frontmatter_title(raw: &str, desired: &str) -> String {
    use rust_tunnel_wiki_core::frontmatter::{extract_frontmatter_delimiter, parse_frontmatter};

    let fm = parse_frontmatter(raw);
    if fm.title.as_deref() == Some(desired) {
        return raw.to_owned();
    }
    let body_without_fm = if let Some((_, end)) = extract_frontmatter_delimiter(raw) {
        raw[end..].trim_start_matches(['\n', '\r']).to_owned()
    } else {
        raw.to_owned()
    };

    let mut yaml = String::from("---\n");
    // `serde_json::to_string` 产生带引号的字符串，合法 YAML 标量
    let esc_title = serde_json::to_string(desired).unwrap_or_else(|_| format!("\"{desired}\""));
    let _ = writeln!(yaml, "title: {esc_title}");

    if let Some(ref_id) = fm.ref_id {
        let esc = serde_json::to_string(ref_id.as_str())
            .unwrap_or_else(|_| format!("\"{}\"", ref_id.as_str()));
        let _ = writeln!(yaml, "ref: {esc}");
    }
    if !fm.aliases.is_empty() {
        yaml.push_str("aliases:\n");
        for a in &fm.aliases {
            let esc = serde_json::to_string(a).unwrap_or_else(|_| format!("\"{a}\""));
            let _ = writeln!(yaml, "  - {esc}");
        }
    }
    if !fm.tags.is_empty() {
        yaml.push_str("tags:\n");
        for tag in &fm.tags {
            let esc = serde_json::to_string(tag).unwrap_or_else(|_| format!("\"{tag}\""));
            let _ = writeln!(yaml, "  - {esc}");
        }
    }
    if let serde_json::Value::Object(map) = fm.extra {
        for (k, v) in map {
            match v {
                serde_json::Value::String(s) => {
                    let esc = serde_json::to_string(&s).unwrap_or_else(|_| format!("\"{s}\""));
                    let _ = writeln!(yaml, "{k}: {esc}");
                }
                serde_json::Value::Number(n) => {
                    let _ = writeln!(yaml, "{k}: {n}");
                }
                serde_json::Value::Bool(b) => {
                    let _ = writeln!(yaml, "{k}: {b}");
                }
                serde_json::Value::Array(arr) => {
                    let _ = writeln!(yaml, "{k}:");
                    for elem in arr {
                        match elem {
                            serde_json::Value::String(s) => {
                                let esc = serde_json::to_string(&s)
                                    .unwrap_or_else(|_| format!("\"{s}\""));
                                let _ = writeln!(yaml, "  - {esc}");
                            }
                            other => {
                                let _ = writeln!(yaml, "  - {other}");
                            }
                        }
                    }
                }
                serde_json::Value::Object(o) => {
                    let j = serde_json::to_string(&serde_json::Value::Object(o))
                        .unwrap_or_else(|_| "{}".to_owned());
                    let _ = writeln!(yaml, "{k}: {j}");
                }
                serde_json::Value::Null => {}
            }
        }
    }
    yaml.push_str("---\n");
    if body_without_fm.is_empty() {
        yaml
    } else {
        yaml.push('\n');
        yaml.push_str(&body_without_fm);
        yaml
    }
}

/// 将 `SystemTime` 转为 unix 秒，失败回退 0（保留供未来复用）。
#[allow(dead_code)]
fn system_time_to_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

// ---------------------------------------------------------------------------
// 7 个操作
// ---------------------------------------------------------------------------

/// 获取 vault 信息（根路径字符串 + 笔记数）。
///
/// # Errors
///
/// `root` 不存在时返回 [`IpcError::VaultNotFound`]。
pub fn get_vault_info(root: &Path) -> IpcResult<VaultInfo> {
    if !root.exists() {
        return Err(IpcError::VaultNotFound(root.display().to_string()));
    }
    let scanner = VaultScanner::new(root);
    let keys = scanner.scan();
    Ok(VaultInfo {
        root: root.display().to_string(),
        note_count: keys.len(),
    })
}

/// 列出全部笔记摘要，按 `modified` 降序、同秒按 `key` 升序.
///
/// # Errors
///
/// `root` 不存在时返回 [`IpcError::VaultNotFound`]。
pub fn list_notes(root: &Path) -> IpcResult<Vec<NoteSummary>> {
    if !root.exists() {
        return Err(IpcError::VaultNotFound(root.display().to_string()));
    }
    let notes = Vault::load(root);
    let mut out: Vec<NoteSummary> = notes.iter().map(NoteSummary::from).collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.key.cmp(&b.key)));
    Ok(out)
}

/// 读取单篇笔记。
///
/// # Errors
///
/// - 路径逃逸 / 非法参数
/// - 笔记不存在
pub fn get_note(root: &Path, key: &str) -> IpcResult<NoteDto> {
    let _guard = resolve_note_path(root, key)?;
    if !root.exists() {
        return Err(IpcError::VaultNotFound(root.display().to_string()));
    }
    let notes = Vault::load(root);
    let Some(note) = notes.iter().find(|n| n.key.as_str() == key) else {
        return Err(IpcError::NoteNotFound(key.to_owned()));
    };
    Ok(NoteDto::from(note))
}

/// 保存笔记（创建或覆盖），可选地注入 `title` 到 frontmatter。
///
/// # Errors
///
/// 路径逃逸、IO 失败等。
#[allow(clippy::needless_pass_by_value)]
pub fn save_note(root: &Path, key: &str, body: &str, title: Option<String>) -> IpcResult<NoteDto> {
    let target = resolve_note_path(root, key)?;
    let to_write = if let Some(desired) = title.as_deref() {
        merge_frontmatter_title(body, desired)
    } else {
        body.to_owned()
    };

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(IpcError::Io)?;
    }
    fs::write(&target, to_write.as_bytes()).map_err(IpcError::Io)?;

    // 同步处理 `.markdown` 残留：若存在同 key 的 `.markdown` 文件且与 `.md` 不是同一文件，删除旧文件以避免二义性
    let alt = root.join(key.replace('\\', "/")).with_extension("markdown");
    if alt != target && alt.exists() {
        let _ = fs::remove_file(&alt);
    }

    get_note(root, key)
}

/// 删除笔记。
///
/// # Errors
///
/// 路径逃逸或笔记不存在。
pub fn delete_note(root: &Path, key: &str) -> IpcResult<()> {
    let target = resolve_note_path(root, key)?;
    let alt = root.join(key.replace('\\', "/")).with_extension("markdown");
    let exists_md = target.exists();
    let exists_markdown = alt != target && alt.exists();
    if !exists_md && !exists_markdown {
        return Err(IpcError::NoteNotFound(key.to_owned()));
    }
    if exists_md {
        fs::remove_file(&target).map_err(IpcError::Io)?;
    }
    if exists_markdown {
        fs::remove_file(&alt).map_err(IpcError::Io)?;
    }
    Ok(())
}

/// 搜索笔记（`search` feature 走 tantivy 临时索引，否则朴素子串匹配）。
///
/// # Errors
///
/// `root` 不存在或检索失败时返回 [`IpcError`]。
#[cfg(feature = "search")]
pub fn search_notes(root: &Path, query: &str, limit: usize) -> IpcResult<Vec<SearchHitDto>> {
    let trimmed = query.trim();
    if trimmed.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    if !root.exists() {
        return Err(IpcError::VaultNotFound(root.display().to_string()));
    }
    let notes = Vault::load(root);
    if notes.is_empty() {
        return Ok(Vec::new());
    }

    // 使用 `std::env::temp_dir()` + 唯一子目录，避免引入 `tempfile` 生产依赖
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let tmp_dir = std::env::temp_dir().join(format!("wiki-search-{pid}-{nanos}"));
    fs::create_dir_all(&tmp_dir).map_err(IpcError::Io)?;

    let res: IpcResult<Vec<SearchHitDto>> = (|| {
        let mut index =
            rust_tunnel_wiki_core::search::SearchIndex::open(&tmp_dir).map_err(IpcError::Search)?;
        for note in &notes {
            index.add_note(note).map_err(IpcError::Search)?;
        }
        let hits = index.search(trimmed, limit).map_err(IpcError::Search)?;
        Ok(hits.iter().map(SearchHitDto::from).collect())
    })();

    let _ = fs::remove_dir_all(&tmp_dir);
    res
}

/// 搜索笔记（无 `search` feature 的朴素降级：大小写不敏感子串，标题权重高于正文）。
///
/// # Errors
///
/// `root` 不存在时返回 [`IpcError::VaultNotFound`]。
#[cfg(not(feature = "search"))]
pub fn search_notes(root: &Path, query: &str, limit: usize) -> IpcResult<Vec<SearchHitDto>> {
    let trimmed = query.trim();
    if trimmed.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    if !root.exists() {
        return Err(IpcError::VaultNotFound(root.display().to_string()));
    }
    let notes = Vault::load(root);
    let lower_q = trimmed.to_lowercase();
    let mut hits: Vec<SearchHitDto> = Vec::new();

    for note in &notes {
        let lower_title = note.title.to_lowercase();
        let lower_body = note.body.to_lowercase();
        let title_hit = lower_title.contains(&lower_q);
        let body_hit = lower_body.contains(&lower_q);
        if !title_hit && !body_hit {
            continue;
        }
        let mut score = 0.0;
        if title_hit {
            score += 2.0;
        }
        if body_hit {
            score += 1.0;
        }
        let snippet = if body_hit {
            if let Some(pos) = lower_body.find(&lower_q) {
                // 40 字符前、160 字符后
                let char_pos = lower_body[..pos].chars().count();
                let q_chars = lower_q.chars().count();
                let body_chars: Vec<char> = note.body.chars().collect();
                let len = body_chars.len();
                let start = char_pos.saturating_sub(40);
                let end = (char_pos + q_chars + 160).min(len);
                let slice: String = body_chars[start..end].iter().collect();
                let mut s = slice;
                if start > 0 {
                    s = format!("…{s}");
                }
                if end < len {
                    s.push('…');
                }
                s
            } else {
                note.body.chars().take(200).collect()
            }
        } else {
            note.body.chars().take(200).collect()
        };
        hits.push(SearchHitDto {
            note_key: note.key.as_str().to_owned(),
            title: note.title.clone(),
            snippet,
            score,
        });
    }

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.note_key.cmp(&b.note_key))
    });
    hits.truncate(limit);
    Ok(hits)
}

/// 首启自举：确保 `root` 存在且非空。
///
/// - 若 `root` 不存在则 `create_dir_all`。
/// - 若 vault 为空（无笔记）则写入一篇欢迎笔记 `welcome.md`，让首次启动非空白。
/// - 已存在笔记时不覆盖任何文件；`welcome.md` 已存在时同样不覆盖。
/// - 幂等：重复调用不产生副作用。
///
/// # Errors
///
/// 目录创建或欢迎笔记写入失败时返回 [`IpcError::Io`]。
pub fn ensure_vault_ready(root: &Path) -> IpcResult<()> {
    fs::create_dir_all(root).map_err(IpcError::Io)?;
    let notes = Vault::load(root);
    if !notes.is_empty() {
        return Ok(());
    }
    // vault 为空，检查 `welcome.md` 是否已存在（理论上不应，因 notes 为空）
    let welcome = root.join("welcome.md");
    if welcome.exists() {
        return Ok(());
    }
    let alt = root.join("welcome.markdown");
    if alt.exists() {
        return Ok(());
    }
    // 不经 `save_note`，避免二次 `Vault::load` 与标题注入干扰；直接写入预设内容
    let body = WELCOME_NOTE_BODY;
    fs::write(&welcome, body.as_bytes()).map_err(IpcError::Io)?;
    Ok(())
}

/// 欢迎笔记正文（带 frontmatter `title: Welcome`）。
const WELCOME_NOTE_BODY: &str = concat!(
    "---\n",
    "title: Welcome\n",
    "---\n",
    "\n",
    "# Welcome to Wiki Desktop\n",
    "\n",
    "This is your first note. Edit it, delete it, or create new notes in this vault.\n",
    "\n",
    "## Quick start\n",
    "\n",
    "- Create a note `getting-started` and link to it: [[getting-started]]\n",
    "- Use `[[links]]` to connect notes and explore the graph view.\n",
    "- All notes are plain Markdown files under your vault directory.\n",
);

/// 获取链接图（节点 + 去重排序后的有向边）。
///
/// # Errors
///
/// `root` 不存在时返回 [`IpcError::VaultNotFound`]。
pub fn get_graph(root: &Path) -> IpcResult<GraphDto> {
    if !root.exists() {
        return Err(IpcError::VaultNotFound(root.display().to_string()));
    }
    let notes = Vault::load(root);
    let graph = LinkGraph::new(notes);

    // 用 `note_list` 命名避免与 `nodes` 触发 similar_names
    let mut graph_nodes: Vec<GraphNode> = graph
        .nodes
        .values()
        .map(|n| GraphNode {
            key: n.key.as_str().to_owned(),
            title: n.title.clone(),
        })
        .collect();
    graph_nodes.sort_by(|a, b| a.key.cmp(&b.key));

    let mut edges: Vec<GraphEdge> = Vec::new();
    {
        use petgraph::visit::EdgeRef as _;
        for edge in graph.edges.edge_references() {
            let Some(src) = graph.edges.node_weight(edge.source()) else {
                continue;
            };
            let Some(dst) = graph.edges.node_weight(edge.target()) else {
                continue;
            };
            edges.push(GraphEdge {
                from: src.as_str().to_owned(),
                to: dst.as_str().to_owned(),
            });
        }
    }
    edges.sort();
    edges.dedup();

    Ok(GraphDto {
        nodes: graph_nodes,
        edges,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_file(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, content).expect("write");
    }

    #[test]
    fn resolve_note_path_accepts_valid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = resolve_note_path(dir.path(), "a/b/c").expect("valid");
        assert!(p.ends_with("a/b/c.md") || p.ends_with("a\\b\\c.md"));
        assert!(p.starts_with(dir.path()));
    }

    #[test]
    fn resolve_note_path_rejects_traversal() {
        let dir = tempfile::tempdir().expect("tempdir");
        for bad in [
            "../../etc/passwd",
            "/etc/passwd",
            "a/../../b",
            "",
            "   ",
            "..\\a",
            "a\\..\\b",
            "a/b/../c",
            "a:bad",
        ] {
            let res = resolve_note_path(dir.path(), bad);
            assert!(res.is_err(), "应拒绝 {bad:?} 实际 {res:?}");
        }
    }

    #[test]
    fn resolve_note_path_rejects_absolute_windows_backslash() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(resolve_note_path(dir.path(), "\\a\\b").is_err());
    }

    #[test]
    fn get_vault_info_empty_and_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        let info = get_vault_info(dir.path()).expect("info");
        assert_eq!(info.note_count, 0);
        write_file(dir.path(), "a.md", "# A");
        write_file(dir.path(), "b.markdown", "# B");
        let info2 = get_vault_info(dir.path()).expect("info2");
        assert_eq!(info2.note_count, 2);
    }

    #[test]
    fn get_vault_info_missing_root_errors() {
        let res = get_vault_info(Path::new("/non/existent/wiki-vault-xyz-12345"));
        assert!(res.is_err());
    }

    #[test]
    fn list_notes_empty_vault() {
        let dir = tempfile::tempdir().expect("tempdir");
        let list = list_notes(dir.path()).expect("list");
        assert!(list.is_empty());
    }

    #[test]
    fn list_notes_sorted_by_modified_then_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "b.md", "body b");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_file(dir.path(), "a.md", "body a");
        let list = list_notes(dir.path()).expect("list");
        assert_eq!(list.len(), 2);
        for w in list.windows(2) {
            let a = &w[0];
            let b = &w[1];
            assert!(
                a.modified > b.modified || (a.modified == b.modified && a.key <= b.key),
                "排序违反：{a:?} vs {b:?}"
            );
        }
    }

    #[test]
    fn list_notes_same_second_key_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "z.md", "z");
        write_file(dir.path(), "a.md", "a");
        write_file(dir.path(), "m.md", "m");
        let list = list_notes(dir.path()).expect("list");
        for w in list.windows(2) {
            let a = &w[0];
            let b = &w[1];
            assert!(
                a.modified > b.modified || (a.modified == b.modified && a.key <= b.key),
                "排序违反：{a:?} vs {b:?}"
            );
        }
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn get_note_found_and_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "hello.md", "---\ntitle: Hello\n---\nBody");
        let dto = get_note(dir.path(), "hello").expect("found");
        assert_eq!(dto.key, "hello");
        assert_eq!(dto.title, "Hello");
        assert!(dto.body.contains("Body"));
        let err = get_note(dir.path(), "missing").expect_err("应不存在");
        assert!(matches!(err, IpcError::NoteNotFound(_)));
    }

    #[test]
    fn get_note_path_traversal_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let res = get_note(dir.path(), "../etc/passwd");
        assert!(matches!(res, Err(IpcError::PathTraversal(_))));
    }

    #[test]
    fn save_note_creates_new() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dto = save_note(dir.path(), "new/note", "hello body", None).expect("save");
        assert_eq!(dto.key, "new/note");
        assert!(dir.path().join("new/note.md").exists());
        let list = list_notes(dir.path()).expect("list");
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn save_note_overwrites_existing() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "a", "first", None).expect("first");
        let dto = save_note(dir.path(), "a", "second", None).expect("second");
        assert!(dto.body.contains("second"));
        let loaded = get_note(dir.path(), "a").expect("get");
        assert!(loaded.body.contains("second"));
    }

    #[test]
    fn save_note_creates_nested_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "x/y/z", "deep", None).expect("deep");
        assert!(dir.path().join("x/y/z.md").exists());
    }

    #[test]
    fn save_note_frontmatter_title_injected() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(
            dir.path(),
            "t1",
            "body without fm",
            Some("My Title".to_owned()),
        )
        .expect("save");
        let content = fs::read_to_string(dir.path().join("t1.md")).expect("read");
        assert!(content.contains("title:"), "应含 title 字段：{content}");
        assert!(content.contains("My Title"));
        let dto = get_note(dir.path(), "t1").expect("get");
        assert_eq!(dto.title, "My Title");

        write_file(dir.path(), "t2.md", "---\ntitle: Old\n---\nbody");
        save_note(
            dir.path(),
            "t2",
            "---\ntitle: Old\n---\nbody",
            Some("New".to_owned()),
        )
        .expect("save2");
        let dto2 = get_note(dir.path(), "t2").expect("get2");
        assert_eq!(dto2.title, "New");
        let content2 = fs::read_to_string(dir.path().join("t2.md")).expect("read2");
        assert!(content2.contains("New"));
    }

    #[test]
    fn save_note_frontmatter_title_same_no_rewrite_needed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = "---\ntitle: Same\n---\nhello";
        save_note(dir.path(), "s", body, Some("Same".to_owned())).expect("save");
        let dto = get_note(dir.path(), "s").expect("get");
        assert_eq!(dto.title, "Same");
    }

    #[test]
    fn merge_frontmatter_title_preserves_tags_and_extra() {
        let raw = "---\ntitle: Old\ntags:\n  - t1\ncustom: 123\n---\nbody";
        let merged = merge_frontmatter_title(raw, "New");
        assert!(merged.contains("title: \"New\"") || merged.contains("title: New"));
        assert!(merged.contains("t1"), "应保留 tags");
        assert!(merged.contains("custom"), "应保留 extra");
        assert!(merged.contains("body"));
    }

    #[test]
    fn merge_frontmatter_title_no_frontmatter_creates_one() {
        let raw = "just body";
        let merged = merge_frontmatter_title(raw, "T");
        assert!(merged.starts_with("---\n"));
        assert!(merged.contains("title:"));
        assert!(merged.contains("just body"));
    }

    #[test]
    fn delete_note_removes_and_list_decreases() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "a", "body a", None).expect("a");
        save_note(dir.path(), "b", "body b", None).expect("b");
        assert_eq!(list_notes(dir.path()).expect("list").len(), 2);
        delete_note(dir.path(), "a").expect("delete");
        let list = list_notes(dir.path()).expect("list2");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].key, "b");
        assert!(!dir.path().join("a.md").exists());
    }

    #[test]
    fn delete_note_missing_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = delete_note(dir.path(), "nope").expect_err("应不存在");
        assert!(matches!(err, IpcError::NoteNotFound(_)));
    }

    #[test]
    fn delete_note_markdown_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "alt.markdown", "hello");
        delete_note(dir.path(), "alt").expect("delete alt");
        assert!(!dir.path().join("alt.markdown").exists());
    }

    #[test]
    fn search_notes_finds_by_title_and_body() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(
            dir.path(),
            "r1",
            "rust is great for systems",
            Some("Rust".to_owned()),
        )
        .expect("r1");
        save_note(
            dir.path(),
            "r2",
            "python is great for scripting",
            Some("Python".to_owned()),
        )
        .expect("r2");
        let hits = search_notes(dir.path(), "rust", 10).expect("search");
        assert!(!hits.is_empty(), "应命中 rust");
        assert!(hits.iter().any(|h| h.note_key == "r1"));
        let hits2 = search_notes(dir.path(), "python", 10).expect("search2");
        assert!(hits2.iter().any(|h| h.note_key == "r2"));
    }

    #[test]
    fn search_notes_empty_query_or_zero_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "a", "hello world", None).expect("a");
        assert!(search_notes(dir.path(), "", 10)
            .expect("empty q")
            .is_empty());
        assert!(search_notes(dir.path(), "   ", 10)
            .expect("blank")
            .is_empty());
        assert!(search_notes(dir.path(), "hello", 0)
            .expect("zero")
            .is_empty());
    }

    #[test]
    fn search_notes_limit_respected() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..5 {
            save_note(
                dir.path(),
                &format!("k{i}"),
                "common body text",
                Some("common".to_owned()),
            )
            .expect("save");
        }
        let hits = search_notes(dir.path(), "common", 2).expect("search");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn get_graph_nodes_and_edges() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "a.md", "link to [[b]]");
        write_file(dir.path(), "b.md", "no links");
        write_file(dir.path(), "c.md", "broken [[missing]]");
        let g = get_graph(dir.path()).expect("graph");
        assert_eq!(g.nodes.len(), 3);
        assert!(g.edges.iter().any(|e| e.from == "a" && e.to == "b"));
        assert!(!g.edges.iter().any(|e| e.from == "c"));
        let mut sorted = g.edges.clone();
        sorted.sort();
        assert_eq!(g.edges, sorted);
    }

    #[test]
    fn get_graph_empty_vault() {
        let dir = tempfile::tempdir().expect("tempdir");
        let g = get_graph(dir.path()).expect("graph");
        assert!(g.nodes.is_empty());
        assert!(g.edges.is_empty());
    }

    #[test]
    fn get_graph_dedup() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "a.md", "[[b]] and [[b]]");
        write_file(dir.path(), "b.md", "x");
        let g = get_graph(dir.path()).expect("graph");
        let ab: Vec<_> = g
            .edges
            .iter()
            .filter(|e| e.from == "a" && e.to == "b")
            .collect();
        assert_eq!(ab.len(), 1);
    }

    #[test]
    fn ensure_vault_ready_creates_missing_root_and_welcome() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("new-vault");
        assert!(!root.exists());
        ensure_vault_ready(&root).expect("ensure");
        assert!(root.is_dir());
        let welcome = root.join("welcome.md");
        assert!(welcome.is_file(), "应创建 welcome.md");
        let content = fs::read_to_string(&welcome).expect("read welcome");
        assert!(content.contains("title: Welcome"));
        assert!(content.contains("Welcome to Wiki Desktop"));
        let info = get_vault_info(&root).expect("info after ensure");
        assert_eq!(info.note_count, 1);
    }

    #[test]
    fn ensure_vault_ready_empty_existing_creates_welcome() {
        let dir = tempfile::tempdir().expect("tempdir");
        ensure_vault_ready(dir.path()).expect("ensure empty");
        assert!(dir.path().join("welcome.md").is_file());
    }

    #[test]
    fn ensure_vault_ready_nonempty_leaves_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "a.md", "hello");
        ensure_vault_ready(dir.path()).expect("ensure nonempty");
        assert!(
            !dir.path().join("welcome.md").exists(),
            "非空 vault 不应创建 welcome"
        );
        assert!(dir.path().join("a.md").exists());
        let info = get_vault_info(dir.path()).expect("info");
        assert_eq!(info.note_count, 1);
    }

    #[test]
    fn ensure_vault_ready_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        ensure_vault_ready(dir.path()).expect("first");
        let content1 = fs::read_to_string(dir.path().join("welcome.md")).expect("read1");
        ensure_vault_ready(dir.path()).expect("second");
        let content2 = fs::read_to_string(dir.path().join("welcome.md")).expect("read2");
        assert_eq!(content1, content2);
        assert_eq!(get_vault_info(dir.path()).expect("info").note_count, 1);
    }

    #[test]
    fn ensure_vault_ready_does_not_overwrite_existing_welcome() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "welcome.md", "---\ntitle: Mine\n---\nhello");
        // 非空（已有 welcome.md），不应覆盖
        ensure_vault_ready(dir.path()).expect("ensure");
        let content = fs::read_to_string(dir.path().join("welcome.md")).expect("read");
        assert!(content.contains("Mine"));
        assert!(!content.contains("Welcome to Wiki Desktop"));
    }
}
