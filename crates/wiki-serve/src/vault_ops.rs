// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

/*!
Vault 操作的纯逻辑层（完全不依赖 `tauri`）。

全部函数签名均为 `fn xxx(root: &Path, ...) -> IpcResult<...>`，便于不带
`tauri` feature 时被 `cargo test` 直接覆盖。
*/

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rust_tunnel_wiki_core::graph::LinkGraph;
use rust_tunnel_wiki_core::vault::{Vault, VaultScanner};

use crate::dto::{
    DeleteFolderResult, FailedEntry, GraphDto, GraphEdge, GraphNode, MovedEntry, NoteDto,
    NoteSummary, RenameFolderResult, SearchHitDto, VaultInfo,
};
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

// ---------------------------------------------------------------------------
// 同步状态与全量拉取（双向同步支撑）
// ---------------------------------------------------------------------------

/// 同步状态文件名（固定在 vault 根目录，点开头故被扫描器忽略）。
const SYNC_STATE_FILE: &str = ".wiki-sync.json";

/// 同步状态 JSON 大小上限（16 MiB，防御性限制）。
const SYNC_STATE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// 读取 `<root>/.wiki-sync.json`；不存在返回 `Ok(None)`。
///
/// # Errors
///
/// 仅在 IO 失败时返回 [`IpcError::Io`]；`root` 不存在时按约定返回 `Ok(None)`。
pub fn read_sync_state(root: &Path) -> IpcResult<Option<String>> {
    if !root.exists() {
        return Ok(None);
    }
    let path = root.join(SYNC_STATE_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path).map_err(IpcError::Io)?;
    Ok(Some(content))
}

/// 原子写入 `<root>/.wiki-sync.json`（临时文件 + `rename`）。
///
/// 路径硬编码为 `root.join(".wiki-sync.json")`，不接受前端路径参数。
/// `json` 大小超过 16 MiB 时返回 [`IpcError::InvalidArgument`]。
///
/// # Errors
///
/// - `root` 不存在时返回 [`IpcError::VaultNotFound`]
/// - `json` 超限时返回 [`IpcError::InvalidArgument`]
/// - 写入或重命名失败时返回 [`IpcError::Io`]
pub fn write_sync_state(root: &Path, json: &str) -> IpcResult<()> {
    if !root.exists() {
        return Err(IpcError::VaultNotFound(root.display().to_string()));
    }
    if json.len() > SYNC_STATE_MAX_BYTES {
        return Err(IpcError::InvalidArgument(format!(
            "sync state 过大：{} 字节，上限 {SYNC_STATE_MAX_BYTES} 字节",
            json.len()
        )));
    }
    let target = root.join(SYNC_STATE_FILE);
    let tmp = root.join(format!("{SYNC_STATE_FILE}.tmp-{}", std::process::id()));
    fs::write(&tmp, json.as_bytes()).map_err(IpcError::Io)?;
    if let Err(err) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(IpcError::Io(err));
    }
    Ok(())
}

/// 一次拿全量笔记（含 `body`/`ref_id`），避免前端 N+1 次 `get_note`。
///
/// 复用 [`Vault::load`] 映射为 [`NoteDto`]，已包含 `ref_id` 与 `body`。
///
/// # Errors
///
/// `root` 不存在时返回 [`IpcError::VaultNotFound`]。
pub fn list_notes_full(root: &Path) -> IpcResult<Vec<NoteDto>> {
    if !root.exists() {
        return Err(IpcError::VaultNotFound(root.display().to_string()));
    }
    let notes = Vault::load(root);
    let out: Vec<NoteDto> = notes.iter().map(NoteDto::from).collect();
    Ok(out)
}

// ---------------------------------------------------------------------------
// 重命名 / 文件夹操作辅助
// ---------------------------------------------------------------------------

/// 校验 `key` / `prefix` 的路径合法性（复用 `resolve_note_path` 的思路）。
fn validate_key_or_prefix(key: &str) -> IpcResult<String> {
    if key.is_empty() {
        return Err(IpcError::PathTraversal("空 key".to_owned()));
    }
    if key.trim().is_empty() {
        return Err(IpcError::PathTraversal("空白 key".to_owned()));
    }
    if Path::new(key).is_absolute() {
        return Err(IpcError::PathTraversal(format!("绝对路径：{key}")));
    }
    let normalized = key.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(IpcError::PathTraversal(format!("绝对路径：{key}")));
    }
    if normalized.contains(':') {
        return Err(IpcError::PathTraversal(format!("非法路径：{key}")));
    }
    // 去除末尾 `/` 后再检查段合法性，允许 `a/b/` 归一为 `a/b`
    let trimmed = normalized.trim_end_matches('/').to_owned();
    if trimmed.is_empty() {
        return Err(IpcError::PathTraversal("空 key".to_owned()));
    }
    if trimmed.split('/').any(|seg| seg.is_empty() || seg == "..") {
        if trimmed.split('/').any(|seg| seg == "..") {
            return Err(IpcError::PathTraversal(format!("路径逃逸：{key}")));
        }
        return Err(IpcError::PathTraversal(format!("非法路径：{key}")));
    }
    Ok(trimmed)
}

/// 查找笔记实际存在的文件（`.md` 优先，其次 `.markdown`）。
fn find_existing_note_file(root: &Path, key: &str) -> Option<PathBuf> {
    let normalized = key.replace('\\', "/");
    let md = root.join(&normalized).with_extension("md");
    if md.exists() {
        return Some(md);
    }
    let markdown = root.join(&normalized).with_extension("markdown");
    if markdown.exists() {
        return Some(markdown);
    }
    None
}

/// 目标路径（统一 `.md`）。
fn target_note_path(root: &Path, key: &str) -> PathBuf {
    let normalized = key.replace('\\', "/");
    root.join(&normalized).with_extension("md")
}

/// 检查目标是否已存在（`.md` 或 `.markdown` 任一存在即视为占用）。
fn target_exists(root: &Path, key: &str) -> bool {
    let normalized = key.replace('\\', "/");
    let md = root.join(&normalized).with_extension("md");
    if md.exists() {
        return true;
    }
    let markdown = root.join(&normalized).with_extension("markdown");
    markdown.exists()
}

/// 对全 vault 笔记执行 `rewrite_wikilinks`，只写回内容变化的文件。
fn rewrite_vault_links(
    root: &Path,
    rename_map: &HashMap<String, String>,
) -> (Vec<String>, usize) {
    if rename_map.is_empty() {
        return (Vec::new(), 0);
    }
    let scanner = VaultScanner::new(root);
    let keys = scanner.scan();
    let mut link_rewritten = Vec::new();
    let mut total = 0usize;
    for key in keys {
        let key_str = key.as_str().to_owned();
        let Some(path) = find_existing_note_file(root, &key_str) else {
            continue;
        };
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let (new_content, count) =
            rust_tunnel_wiki_core::link::rewrite_wikilinks(&content, rename_map);
        if count == 0 || new_content == content {
            continue;
        }
        if fs::write(&path, new_content.as_bytes()).is_ok() {
            link_rewritten.push(key_str);
            total += count;
        }
    }
    link_rewritten.sort();
    (link_rewritten, total)
}

/// 尝试清理空目录链（从 `from_key` 的父目录向上至 `root`，遇非空即停，不删 `root`）。
fn cleanup_empty_parents(root: &Path, from_key: &str) {
    let normalized = from_key.replace('\\', "/");
    let mut cur = Path::new(&normalized).parent();
    while let Some(parent) = cur {
        if parent.as_os_str().is_empty() {
            break;
        }
        let dir = root.join(parent);
        if !dir.exists() {
            cur = parent.parent();
            continue;
        }
        let is_empty = fs::read_dir(&dir).is_ok_and(|mut it| it.next().is_none());
        if is_empty {
            let _ = fs::remove_dir(&dir);
        } else {
            break;
        }
        cur = parent.parent();
    }
}

/// 重命名单篇笔记（逐文件 `fs::rename`），可选全 vault 链接重写。
///
/// # Errors
///
/// - 路径非法 / 逃逸
/// - 源笔记不存在
/// - 目标已存在
pub fn rename_note(
    root: &Path,
    old_key: &str,
    new_key: &str,
    rewrite_links: bool,
) -> IpcResult<NoteDto> {
    let old_norm = validate_key_or_prefix(old_key)?;
    let new_norm = validate_key_or_prefix(new_key)?;
    if old_norm == new_norm {
        // 大小写仅变体的重命名：仍需处理文件系统大小写不敏感的场景，视为有效
        // 但若完全相等则直接返回
        if old_key == new_key {
            return get_note(root, &old_norm);
        }
    }
    // 校验归一后仍在 root 内
    let old_candidate = root.join(&old_norm).with_extension("md");
    if !old_candidate.starts_with(root) {
        return Err(IpcError::PathTraversal(format!("逃逸到 root 之外：{old_key}")));
    }
    let new_candidate = root.join(&new_norm).with_extension("md");
    if !new_candidate.starts_with(root) {
        return Err(IpcError::PathTraversal(format!("逃逸到 root 之外：{new_key}")));
    }
    let Some(src_path) = find_existing_note_file(root, &old_norm) else {
        return Err(IpcError::NoteNotFound(old_key.to_owned()));
    };
    if target_exists(root, &new_norm) {
        // 若目标即源自身（同 key 不同扩展名的自冲突），视为已存在
        let src_key_lower = old_norm.to_lowercase();
        let new_key_lower = new_norm.to_lowercase();
        if src_key_lower != new_key_lower {
            return Err(IpcError::InvalidArgument(format!(
                "目标已存在：{new_key}"
            )));
        }
    }
    if let Some(parent) = new_candidate.parent() {
        fs::create_dir_all(parent).map_err(IpcError::Io)?;
    }
    fs::rename(&src_path, &new_candidate).map_err(IpcError::Io)?;
    // 若源是 `.markdown` 且目标为 `.md`，`fs::rename` 已移动；无需额外清理
    // 但若源目录下残留同 key 的另一种扩展名（异常双文件），删除之
    let old_alt = root
        .join(old_norm.replace('\\', "/"))
        .with_extension("markdown");
    if old_alt != src_path && old_alt.exists() {
        let _ = fs::remove_file(&old_alt);
    }
    // 清理空父目录
    cleanup_empty_parents(root, &old_norm);

    if rewrite_links {
        let mut rename_map = HashMap::new();
        rename_map.insert(old_norm.to_lowercase(), new_norm.clone());
        let (_rewritten, _count) = rewrite_vault_links(root, &rename_map);
    }

    get_note(root, &new_norm)
}

/// 重命名文件夹（批量 `fs::rename`），可选全 vault 链接重写。
#[allow(clippy::too_many_lines)]
///
/// # Errors
///
/// - 路径非法 / 逃逸
/// - `old_prefix` 未命中任何笔记
/// - `new_prefix` 为 `old_prefix` 的子路径
pub fn rename_folder(
    root: &Path,
    old_prefix: &str,
    new_prefix: &str,
    rewrite_links: bool,
) -> IpcResult<RenameFolderResult> {
    let old_norm = validate_key_or_prefix(old_prefix)?;
    let new_norm = validate_key_or_prefix(new_prefix)?;
    if old_norm == new_norm {
        return Err(IpcError::InvalidArgument("前后缀相同，无需移动".to_owned()));
    }
    if new_norm == old_norm
        || new_norm.starts_with(&format!("{old_norm}/"))
    {
        return Err(IpcError::InvalidArgument(format!(
            "新前缀不能是旧前缀的子路径：{new_prefix} 在 {old_prefix} 之内"
        )));
    }
    let old_candidate = root.join(&old_norm).with_extension("md");
    if !old_candidate.starts_with(root) {
        return Err(IpcError::PathTraversal(format!("逃逸到 root 之外：{old_prefix}")));
    }
    let new_candidate = root.join(&new_norm).with_extension("md");
    if !new_candidate.starts_with(root) {
        return Err(IpcError::PathTraversal(format!("逃逸到 root 之外：{new_prefix}")));
    }

    let scanner = VaultScanner::new(root);
    let all_keys = scanner.scan();
    let mut matched: Vec<String> = all_keys
        .iter()
        .map(|k| k.as_str().to_owned())
        .filter(|k| k == &old_norm || k.starts_with(&format!("{old_norm}/")))
        .collect();
    if matched.is_empty() {
        return Err(IpcError::NoteNotFound(old_prefix.to_owned()));
    }
    matched.sort();

    // 预计算目标 key，检测批内冲突与已存在文件的冲突
    let mut seen_targets: HashSet<String> = HashSet::new();
    let mut planned: Vec<(String, String)> = Vec::new();
    for key in &matched {
        let new_key = if key == &old_norm {
            new_norm.clone()
        } else {
            // SAFETY: 前缀匹配已保证
            let suffix = &key[old_norm.len()..];
            format!("{new_norm}{suffix}")
        };
        planned.push((key.clone(), new_key));
    }

    let mut moved: Vec<MovedEntry> = Vec::new();
    let mut failed: Vec<FailedEntry> = Vec::new();

    for (from_key, to_key) in planned {
        let to_lower = to_key.to_lowercase();
        if !seen_targets.insert(to_lower.clone()) {
            failed.push(FailedEntry {
                key: from_key.clone(),
                error: format!("批内目标冲突：{to_key}"),
            });
            continue;
        }
        if target_exists(root, &to_key) {
            // 若目标已在 matched 集合中（即同批内将被移走），允许覆盖检查延后；
            // 但此处扫描基于移动前状态，无法区分，故一律视为占用
            failed.push(FailedEntry {
                key: from_key.clone(),
                error: format!("目标已存在：{to_key}"),
            });
            continue;
        }
        let Some(src_path) = find_existing_note_file(root, &from_key) else {
            failed.push(FailedEntry {
                key: from_key.clone(),
                error: "源文件不存在".to_owned(),
            });
            continue;
        };
        let dst_path = target_note_path(root, &to_key);
        if let Some(parent) = dst_path.parent() {
            if let Err(err) = fs::create_dir_all(parent) {
                failed.push(FailedEntry {
                    key: from_key.clone(),
                    error: err.to_string(),
                });
                continue;
            }
        }
        match fs::rename(&src_path, &dst_path) {
            Ok(()) => {
                // 清理另一种扩展名的残留
                let old_alt = root.join(from_key.replace('\\', "/")).with_extension("markdown");
                if old_alt != src_path && old_alt.exists() {
                    let _ = fs::remove_file(&old_alt);
                }
                moved.push(MovedEntry {
                    from_key: from_key.clone(),
                    to_key: to_key.clone(),
                });
            }
            Err(err) => {
                failed.push(FailedEntry {
                    key: from_key.clone(),
                    error: err.to_string(),
                });
            }
        }
    }

    // 清理空目录链（对每个成功移动的源路径）
    for m in &moved {
        cleanup_empty_parents(root, &m.from_key);
    }

    let (link_rewritten, rewritten_count) = if rewrite_links && !moved.is_empty() {
        let mut rename_map: HashMap<String, String> = HashMap::new();
        for m in &moved {
            rename_map.insert(m.from_key.to_lowercase(), m.to_key.clone());
        }
        rewrite_vault_links(root, &rename_map)
    } else {
        (Vec::new(), 0)
    };

    moved.sort_by(|a, b| a.from_key.cmp(&b.from_key));
    failed.sort_by(|a, b| a.key.cmp(&b.key));

    Ok(RenameFolderResult {
        moved,
        failed,
        link_rewritten,
        rewritten_count,
    })
}

/// 删除文件夹下的全部笔记，并清理空目录链。
///
/// # Errors
///
/// 路径非法 / 逃逸时返回 [`IpcError`]。
pub fn delete_folder(root: &Path, prefix: &str) -> IpcResult<DeleteFolderResult> {
    let norm = validate_key_or_prefix(prefix)?;
    let candidate = root.join(&norm).with_extension("md");
    if !candidate.starts_with(root) {
        return Err(IpcError::PathTraversal(format!("逃逸到 root 之外：{prefix}")));
    }
    let scanner = VaultScanner::new(root);
    let all_keys = scanner.scan();
    let matched: Vec<String> = all_keys
        .iter()
        .map(|k| k.as_str().to_owned())
        .filter(|k| k == &norm || k.starts_with(&format!("{norm}/")))
        .collect();

    let mut deleted: Vec<String> = Vec::new();
    let mut failed: Vec<FailedEntry> = Vec::new();

    for key in matched {
        let md = root.join(key.replace('\\', "/")).with_extension("md");
        let markdown = root.join(key.replace('\\', "/")).with_extension("markdown");
        let exists_md = md.exists();
        let exists_markdown = markdown != md && markdown.exists();
        if !exists_md && !exists_markdown {
            failed.push(FailedEntry {
                key: key.clone(),
                error: "文件不存在".to_owned(),
            });
            continue;
        }
        let mut ok = true;
        if exists_md {
            if let Err(err) = fs::remove_file(&md) {
                failed.push(FailedEntry {
                    key: key.clone(),
                    error: err.to_string(),
                });
                ok = false;
            }
        }
        if exists_markdown {
            if let Err(err) = fs::remove_file(&markdown) {
                // 若 md 已删但 markdown 失败，视为失败
                if ok {
                    failed.push(FailedEntry {
                        key: key.clone(),
                        error: err.to_string(),
                    });
                    ok = false;
                }
            }
        }
        if ok {
            deleted.push(key.clone());
        }
    }

    for key in &deleted {
        cleanup_empty_parents(root, key);
    }
    // 若前缀本身是目录（无笔记但有空目录），也尝试清理
    let dir = root.join(norm.replace('\\', "/"));
    if dir.is_dir() {
        let is_empty = fs::read_dir(&dir).is_ok_and(|mut it| it.next().is_none());
        if is_empty {
            let _ = fs::remove_dir(&dir);
            // 继续向上清理空父目录链
            if let Some(parent) = Path::new(&norm.replace('\\', "/")).parent() {
                if !parent.as_os_str().is_empty() {
                    // 构造一个虚拟 key 以复用清理逻辑
                    let virtual_key = format!("{}/__dummy__", parent.display());
                    cleanup_empty_parents(root, &virtual_key);
                }
            }
        }
    }

    deleted.sort();
    failed.sort_by(|a, b| a.key.cmp(&b.key));

    Ok(DeleteFolderResult { deleted, failed })
}

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

    #[test]
    fn rename_note_moves_and_readable() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "a/note", "hello body", None).expect("save");
        let dto = rename_note(dir.path(), "a/note", "b/note", false).expect("rename");
        assert_eq!(dto.key, "b/note");
        assert!(!dir.path().join("a/note.md").exists());
        assert!(dir.path().join("b/note.md").exists());
        let got = get_note(dir.path(), "b/note").expect("get after rename");
        assert!(got.body.contains("hello"));
        assert!(matches!(
            get_note(dir.path(), "a/note"),
            Err(IpcError::NoteNotFound(_))
        ));
    }

    #[test]
    fn rename_note_target_conflict_goes_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "a", "body a", None).expect("a");
        save_note(dir.path(), "b", "body b", None).expect("b");
        let err = rename_note(dir.path(), "a", "b", false).expect_err("应冲突");
        assert!(matches!(err, IpcError::InvalidArgument(_)));
        // 原文件仍在
        assert!(dir.path().join("a.md").exists());
    }

    #[test]
    fn rename_note_markdown_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "old.markdown", "hello");
        let dto = rename_note(dir.path(), "old", "new", false).expect("rename markdown");
        assert_eq!(dto.key, "new");
        assert!(dir.path().join("new.md").exists());
        assert!(!dir.path().join("old.markdown").exists());
    }

    #[test]
    fn rename_note_rewrite_links() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "a", "body", None).expect("a");
        save_note(dir.path(), "other", "link [[a]] and [[a|alias]]", None).expect("other");
        rename_note(dir.path(), "a", "b", true).expect("rename with rewrite");
        let other = get_note(dir.path(), "other").expect("other after");
        assert!(other.body.contains("[[b]]"), "应重写：{}", other.body);
        assert!(other.body.contains("[[b|alias]]"), "alias 保留：{}", other.body);
        // rewrite=false 时不改
        let dir2 = tempfile::tempdir().expect("tempdir");
        save_note(dir2.path(), "a", "body", None).expect("a2");
        save_note(dir2.path(), "other", "link [[a]]", None).expect("other2");
        rename_note(dir2.path(), "a", "b", false).expect("rename no rewrite");
        let other2 = get_note(dir2.path(), "other").expect("other2 after");
        assert!(other2.body.contains("[[a]]"), "不重写时保留：{}", other2.body);
    }

    #[test]
    fn rename_note_rewrite_outside_folder() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "folder/note", "body", None).expect("note");
        save_note(dir.path(), "outside", "ref [[folder/note]]", None).expect("outside");
        rename_note(dir.path(), "folder/note", "folder/renamed", true).expect("rename");
        let outside = get_note(dir.path(), "outside").expect("outside after");
        assert!(
            outside.body.contains("[[folder/renamed]]"),
            "文件夹外链接应改写：{}",
            outside.body
        );
    }

    #[test]
    fn rename_folder_moves_nested_and_cleans_empty_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "docs/a", "body a", None).expect("a");
        save_note(dir.path(), "docs/b/c", "body c", None).expect("c");
        save_note(dir.path(), "other", "link [[docs/a]]", None).expect("other");
        let res = rename_folder(dir.path(), "docs", "notes", true).expect("rename folder");
        assert_eq!(res.moved.len(), 2);
        assert!(res.failed.is_empty());
        assert!(dir.path().join("notes/a.md").exists());
        assert!(dir.path().join("notes/b/c.md").exists());
        assert!(!dir.path().join("docs").exists(), "空目录应清理");
        let other = get_note(dir.path(), "other").expect("other after");
        assert!(other.body.contains("[[notes/a]]"), "链接应改写：{}", other.body);
        assert!(!res.link_rewritten.is_empty());
        assert!(res.rewritten_count >= 1);
    }

    #[test]
    fn rename_folder_target_conflict_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "src/a", "body a", None).expect("a");
        save_note(dir.path(), "dst/a", "body dst", None).expect("dst");
        let res = rename_folder(dir.path(), "src", "dst", false).expect("rename");
        assert!(res.moved.is_empty());
        assert_eq!(res.failed.len(), 1);
        assert_eq!(res.failed[0].key, "src/a");
        assert!(dir.path().join("src/a.md").exists());
    }

    #[test]
    fn rename_folder_markdown_extension_and_rewrite_toggle() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "old/x.markdown", "hello");
        write_file(dir.path(), "other.md", "link [[old/x]]");
        let res = rename_folder(dir.path(), "old", "new", true).expect("rename");
        assert_eq!(res.moved.len(), 1);
        assert!(dir.path().join("new/x.md").exists());
        assert!(get_note(dir.path(), "other").expect("other").body.contains("[[new/x]]"));
        // rewrite=false
        let dir2 = tempfile::tempdir().expect("tempdir");
        write_file(dir2.path(), "old/x.md", "hello");
        write_file(dir2.path(), "other.md", "link [[old/x]]");
        let res2 = rename_folder(dir2.path(), "old", "new2", false).expect("rename2");
        assert_eq!(res2.moved.len(), 1);
        assert!(res2.link_rewritten.is_empty());
        assert_eq!(res2.rewritten_count, 0);
        assert!(
            get_note(dir2.path(), "other")
                .expect("other2")
                .body
                .contains("[[old/x]]")
        );
    }

    #[test]
    fn rename_folder_not_found_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = rename_folder(dir.path(), "missing", "new", false).expect_err("应不存在");
        assert!(matches!(err, IpcError::NoteNotFound(_)));
    }

    #[test]
    fn rename_folder_self_containment_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "a/b", "body", None).expect("a/b");
        let err = rename_folder(dir.path(), "a", "a/b", false).expect_err("子路径应拒绝");
        assert!(matches!(err, IpcError::InvalidArgument(_)));
    }

    #[test]
    fn rename_folder_rewrite_outside_folder_links() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "docs/a", "body", None).expect("a");
        save_note(dir.path(), "outside", "ref [[docs/a]] and [[docs/a|alias]]", None)
            .expect("outside");
        let res = rename_folder(dir.path(), "docs", "notes", true).expect("rename");
        assert_eq!(res.moved.len(), 1);
        let outside = get_note(dir.path(), "outside").expect("outside after");
        assert!(outside.body.contains("[[notes/a]]"), "应改写：{}", outside.body);
    }

    #[test]
    fn delete_folder_removes_and_cleans_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "docs/a", "body a", None).expect("a");
        save_note(dir.path(), "docs/b/c", "body c", None).expect("c");
        save_note(dir.path(), "other", "keep", None).expect("other");
        let res = delete_folder(dir.path(), "docs").expect("delete");
        assert_eq!(res.deleted.len(), 2);
        assert!(res.failed.is_empty());
        assert!(res.deleted.contains(&"docs/a".to_owned()));
        assert!(res.deleted.contains(&"docs/b/c".to_owned()));
        assert!(!dir.path().join("docs").exists(), "空目录链应清理");
        assert!(dir.path().join("other.md").exists());
    }

    #[test]
    fn delete_folder_wrong_prefix_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "a/b", "body", None).expect("a/b");
        let res = delete_folder(dir.path(), "missing").expect("delete missing");
        assert!(res.deleted.is_empty());
        assert!(res.failed.is_empty());
    }

    // ---- 同步状态与全量拉取 ----

    #[test]
    fn read_sync_state_missing_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_sync_state(dir.path()).expect("read").is_none());
        // root 不存在也返回 None
        let missing = dir.path().join("no-such-root-xyz");
        assert!(read_sync_state(&missing).expect("missing root").is_none());
    }

    #[test]
    fn write_read_sync_state_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = r#"{"version":1,"entries":{}}"#;
        write_sync_state(dir.path(), json).expect("write");
        let back = read_sync_state(dir.path()).expect("read").expect("some");
        assert_eq!(back, json);
        // 覆盖写入
        let json2 = r#"{"version":1,"entries":{"a":{"ref":"a"}}}"#;
        write_sync_state(dir.path(), json2).expect("write2");
        let back2 = read_sync_state(dir.path()).expect("read2").expect("some2");
        assert_eq!(back2, json2);
    }

    #[test]
    fn sync_state_not_counted_as_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "a", "body a", None).expect("a");
        write_sync_state(dir.path(), r#"{"v":1}"#).expect("write sync");
        // vault 扫描不应把 .wiki-sync.json 当笔记
        let info = get_vault_info(dir.path()).expect("info");
        assert_eq!(info.note_count, 1);
        let list = list_notes(dir.path()).expect("list");
        assert_eq!(list.len(), 1);
        let scanner = VaultScanner::new(dir.path());
        assert_eq!(scanner.scan().len(), 1);
        // 原子写不应残留 tmp 文件
        let has_tmp = fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains(".wiki-sync.json.tmp-"));
        assert!(!has_tmp, "不应残留 tmp 文件");
    }

    #[test]
    fn write_sync_state_missing_root_errors() {
        let err = write_sync_state(Path::new("/non/existent/wiki-sync-root-xyz"), "{}")
            .expect_err("应失败");
        assert!(matches!(err, IpcError::VaultNotFound(_)));
    }

    #[test]
    fn write_sync_state_oversize_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 构造超过 16 MiB 的 json（16*1024*1024 + 1 字节）
        let big = "a".repeat(16 * 1024 * 1024 + 1);
        let err = write_sync_state(dir.path(), &big).expect_err("应超限");
        assert!(matches!(err, IpcError::InvalidArgument(_)));
        // 未写入目标文件
        assert!(!dir.path().join(".wiki-sync.json").exists());
        // 也不残留 tmp
        let has_tmp = fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains(".wiki-sync.json.tmp-"));
        assert!(!has_tmp);
    }

    #[test]
    fn list_notes_full_returns_body_and_ref_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_note(dir.path(), "a", "hello body", None).expect("a");
        save_note(dir.path(), "b", "plain", None).expect("b");
        let full = list_notes_full(dir.path()).expect("full");
        assert_eq!(full.len(), 2);
        let a = full.iter().find(|n| n.key == "a").expect("a");
        assert!(a.body.contains("hello body"));
        // 无 ref 时为 None
        assert!(a.ref_id.is_none());
        // list 摘要同样含 ref_id 字段（None）
        let list = list_notes(dir.path()).expect("list");
        let la = list.iter().find(|n| n.key == "a").expect("la");
        assert!(la.ref_id.is_none());
    }

    #[test]
    fn list_notes_full_frontmatter_ref_mapped() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(
            dir.path(),
            "with-ref.md",
            "---\nref: deploy/prod-checklist\ntitle: T\n---\nbody text",
        );
        write_file(dir.path(), "plain.md", "no frontmatter body");
        let full = list_notes_full(dir.path()).expect("full");
        let hit = full.iter().find(|n| n.key == "with-ref").expect("with-ref");
        assert_eq!(hit.ref_id.as_deref(), Some("deploy/prod-checklist"));
        assert!(hit.body.contains("body text"));
        // 通过 get_note / list_notes 同样映射
        let dto = get_note(dir.path(), "with-ref").expect("get");
        assert_eq!(dto.ref_id.as_deref(), Some("deploy/prod-checklist"));
        let list = list_notes(dir.path()).expect("list");
        let sum = list.iter().find(|n| n.key == "with-ref").expect("sum");
        assert_eq!(sum.ref_id.as_deref(), Some("deploy/prod-checklist"));
        let plain = full.iter().find(|n| n.key == "plain").expect("plain");
        assert!(plain.ref_id.is_none());
    }

    #[test]
    fn dto_ref_id_serde_default_compat() {
        // 旧 JSON 不含 ref_id 字段时应反序列化为 None（保持兼容）
        let json = r#"{"key":"a","title":"T","tags":[],"modified":0}"#;
        let sum: NoteSummary = serde_json::from_str(json).expect("summary");
        assert!(sum.ref_id.is_none());
        let json2 = r#"{"key":"a","title":"T","aliases":[],"tags":[],"body":"b","modified":0}"#;
        let dto: NoteDto = serde_json::from_str(json2).expect("dto");
        assert!(dto.ref_id.is_none());
        // 含 ref_id 时正常解析
        let json3 = r#"{"key":"a","title":"T","tags":[],"modified":0,"ref_id":"a/b"}"#;
        let sum3: NoteSummary = serde_json::from_str(json3).expect("sum3");
        assert_eq!(sum3.ref_id.as_deref(), Some("a/b"));
    }
}
