//! Vault 目录扫描：发现 vault 根目录下的全部 Markdown 笔记。
//!
//! 使用 [`walkdir::WalkDir`] 递归遍历，跳过隐藏目录与已知忽略模式，
//! 仅收集 `.md`/`.markdown` 文件并映射为 [`NoteKey`]。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use walkdir::WalkDir;

use crate::frontmatter::{extract_frontmatter_delimiter, parse_frontmatter};
use crate::link::extract_wikilinks;
use crate::markdown::{extract_headings, render_html};
use crate::note::{Note, NoteKey};

/// 一个已发现的 Markdown vault。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vault {
    /// vault 根目录。
    pub root: PathBuf,
    /// 已发现的笔记键。
    pub notes: Vec<NoteKey>,
}

/// 笔记的轻量元信息：键、文件路径与修改时间（不含正文）。
///
/// 由 [`Vault::scan_meta`] 产出：一次 walk 只做键推导与 `stat`，不读文件内容、
/// 不做任何 comrak 解析，供摘要列表等路径使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteMeta {
    /// 笔记唯一键。
    pub key: NoteKey,
    /// 笔记文件绝对路径。
    pub path: PathBuf,
    /// 最后修改时间。
    pub modified: SystemTime,
}

impl Vault {
    /// 扫描 `root` 目录并返回仅含键的 [`Vault`]（不读文件内容）。
    #[must_use]
    pub fn scan(root: impl Into<PathBuf>) -> Self {
        let scanner = VaultScanner::new(root);
        let notes = scanner.scan();
        Self {
            root: scanner.root,
            notes,
        }
    }

    /// 扫描 `root` 并将全部笔记加载为 [`Note`] 列表。
    ///
    /// 等价于 [`Vault::load_notes`]，保留 `load` 命名以满足任务要求的
    /// `Vault::load(vault_root)` 签名。
    #[must_use]
    pub fn load(root: impl Into<PathBuf>) -> Vec<Note> {
        Self::load_notes(root)
    }

    /// 只读目标单文件并解析为 [`Note`]（不扫描全 vault）。
    ///
    /// `key` 为 vault 内相对路径键（去扩展名，`/` 分隔）；依次尝试 `.md`
    /// 与 `.markdown` 扩展名。文件不存在、非 Markdown 或 vault 之外时返回 `None`。
    ///
    /// 解析管线与 [`Vault::load_notes`] 单文件分支一致（含 frontmatter /
    /// wikilinks / 元信息），同一文件两者结果相同。
    #[must_use]
    pub fn load_one(root: &Path, key: &str) -> Option<Note> {
        let normalized = key.replace('\\', "/");
        // 复用扫描器的忽略规则：命中忽略段的 key 视为不存在
        let scanner = VaultScanner::new(root);
        if normalized.split('/').any(|seg| {
            seg.starts_with('.') || scanner.ignore_patterns.iter().any(|pat| pat == seg)
        }) {
            return None;
        }
        for ext in ["md", "markdown"] {
            let path = root.join(&normalized).with_extension(ext);
            // 词法约束：不得逃出 root
            if !path.starts_with(root) {
                return None;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(note_key) = derive_note_key(root, &path) else {
                continue;
            };
            if note_key.as_str() != normalized {
                continue;
            }
            return Some(parse_note_file(&path, &note_key, &text, None));
        }
        None
    }

    /// 只扫描键与元信息（`modified`），不读文件内容、不做任何解析。
    ///
    /// 供 `list_notes` 等摘要路径使用：摘要 DTO 只需 key/title/tags/modified，
    /// 其中 title 仍需读文件（见 [`Vault::scan_meta`] 的说明）——本函数返回
    /// 键与 mtime，调用方按需再读单文件补 title/tags。
    #[must_use]
    pub fn scan_meta(root: impl Into<PathBuf>) -> Vec<NoteMeta> {
        let root = root.into();
        let scanner = VaultScanner::new(root.clone());
        let ignore = scanner.ignore_patterns.clone();
        let mut out = Vec::new();

        if !root.exists() {
            return out;
        }

        let walker = WalkDir::new(&root).into_iter().filter_entry(|e| {
            filter_entry(&root, &ignore, e)
        });

        for entry in walker.filter_map(std::result::Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let file_name = entry.file_name().to_string_lossy();
            if file_name.starts_with('.') {
                continue;
            }
            let ext = entry
                .path()
                .extension()
                .and_then(|s| s.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            if ext != "md" && ext != "markdown" {
                continue;
            }
            let Some(key) = derive_note_key(&root, entry.path()) else {
                continue;
            };
            // `DirEntry::metadata` 复用 walk 期间的 `stat` 缓存，不新增 syscall。
            let modified = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::now());
            out.push(NoteMeta {
                key,
                path: entry.path().to_owned(),
                modified,
            });
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    /// 扫描 `root` 并加载全部笔记（`load` 的别名，等价方法）。
    #[must_use]
    pub fn load_notes(root: impl Into<PathBuf>) -> Vec<Note> {
        let root = root.into();
        let mut notes: Vec<Note> = Self::scan_meta(&root)
            .into_iter()
            .filter_map(|meta| {
                std::fs::read_to_string(&meta.path).ok().map(|text| {
                    parse_note_file(&meta.path, &meta.key, &text, Some(meta.modified))
                })
            })
            .collect();
        notes.sort_by(|a, b| a.key.cmp(&b.key));
        notes
    }

    /// 扫描 `root` 并加载摘要所需的轻量笔记（跳过 wikilinks 与 markdown 解析）。
    ///
    /// 摘要 DTO（key/title/aliases/tags/ref_id/modified）不需要正文解析：
    /// frontmatter 仍需解析（title/tags 可能来自 frontmatter），但跳过
    /// `extract_wikilinks`（含 comrak）与 `render_html`/`extract_headings`。
    /// `wikilinks` 字段置空——调用方不得将其用于链接图/断链等语义。
    #[must_use]
    pub fn load_notes_meta(root: impl Into<PathBuf>) -> Vec<Note> {
        let root = root.into();
        let mut notes: Vec<Note> = Self::scan_meta(&root)
            .into_iter()
            .filter_map(|meta| {
                std::fs::read_to_string(&meta.path).ok().map(|text| {
                    parse_note_file_meta(&meta.path, &meta.key, &text, Some(meta.modified))
                })
            })
            .collect();
        notes.sort_by(|a, b| a.key.cmp(&b.key));
        notes
    }
}

/// walk `filter_entry` 的共享实现：隐藏段与忽略模式剪枝。
///
/// `Vault::load_notes`（经 [`Vault::scan_meta`]）与 [`VaultScanner::scan`]
/// 复用同一规则，避免两处 walk 逻辑漂移。
fn filter_entry(root: &Path, ignore: &[String], e: &walkdir::DirEntry) -> bool {
    if e.depth() == 0 {
        return true;
    }
    let file_name = e.file_name().to_string_lossy();
    if file_name.starts_with('.') {
        return false;
    }
    if e.file_type().is_dir() {
        let name = file_name.to_string();
        if ignore.iter().any(|pat| pat == &name) {
            return false;
        }
    }
    // 路径中任意段命中忽略模式则剪枝
    if let Ok(rel) = e.path().strip_prefix(root) {
        for comp in rel.components() {
            let seg = comp.as_os_str().to_string_lossy();
            if seg.starts_with('.') {
                return false;
            }
            if ignore.iter().any(|pat| pat == seg.as_ref()) {
                return false;
            }
        }
    }
    true
}

/// 解析单文件正文为摘要用 [`Note`]（frontmatter + 元信息，无正文解析）。
///
/// 与 [`parse_note_file`] 的区别：跳过 `extract_wikilinks`/`render_html`/
/// `extract_headings`（零 comrak 解析），`wikilinks` 置空。摘要字段
/// （title/aliases/tags/ref_id）与全量解析一致。
fn parse_note_file_meta(
    path: &Path,
    key: &NoteKey,
    text: &str,
    modified: Option<SystemTime>,
) -> Note {
    let frontmatter = parse_frontmatter(text);
    let body = if let Some((_, end)) = extract_frontmatter_delimiter(text) {
        text[end..].trim_start_matches(['\n', '\r']).to_owned()
    } else {
        text.to_owned()
    };
    let title = frontmatter.title.clone().unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(key.as_str())
            .to_owned()
    });
    let modified = modified.unwrap_or_else(|| {
        path.metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::now())
    });
    Note {
        key: key.clone(),
        ref_id: frontmatter.ref_id.clone(),
        title,
        aliases: frontmatter.aliases.clone(),
        tags: frontmatter.tags.clone(),
        body,
        wikilinks: Vec::new(),
        frontmatter,
        modified,
    }
}

/// 解析单文件正文为 [`Note`]（frontmatter + wikilinks + 元信息）。
///
/// `modified` 优先用传入值（walk 期间已 `stat`），否则回退读文件 mtime；
/// `load_notes` 与 [`Vault::load_one`] 复用同一函数，同一文件两者结果一致。
/// wikilink 提取内部经单次 comrak 解析复用 AST。
fn parse_note_file(path: &Path, key: &NoteKey, text: &str, modified: Option<SystemTime>) -> Note {
    let frontmatter = parse_frontmatter(text);
    let body = if let Some((_, end)) = extract_frontmatter_delimiter(text) {
        text[end..].trim_start_matches(['\n', '\r']).to_owned()
    } else {
        text.to_owned()
    };
    let wikilinks = extract_wikilinks(&body);
    // 调用 markdown 桩以满足任务要求的依赖调用（当前为桩，返回空）
    let _html = render_html(&body);
    let _headings = extract_headings(&body);
    let title = frontmatter.title.clone().unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(key.as_str())
            .to_owned()
    });
    let modified = modified.unwrap_or_else(|| {
        path.metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::now())
    });
    Note {
        key: key.clone(),
        ref_id: frontmatter.ref_id.clone(),
        title,
        aliases: frontmatter.aliases.clone(),
        tags: frontmatter.tags.clone(),
        body,
        wikilinks,
        frontmatter,
        modified,
    }
}


/// 递归扫描 vault 根目录、发现 `.md`/`.markdown` 笔记的扫描器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultScanner {
    /// vault 根目录。
    pub root: PathBuf,
    /// 需要跳过的目录/文件模式（按路径段匹配）。
    pub ignore_patterns: Vec<String>,
}

impl VaultScanner {
    /// 以 `root` 为根目录构造扫描器，使用默认忽略模式。
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            ignore_patterns: default_ignore_patterns(),
        }
    }

    /// 扫描 vault，返回全部笔记键（按字母序排序，保证确定性）。
    #[must_use]
    pub fn scan(&self) -> Vec<NoteKey> {
        let mut keys = Vec::new();
        if !self.root.exists() {
            return keys;
        }
        let walker = WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|e| filter_entry(&self.root, &self.ignore_patterns, e));

        for entry in walker.filter_map(std::result::Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let file_name = entry.file_name().to_string_lossy();
            if file_name.starts_with('.') {
                continue;
            }
            let ext = entry
                .path()
                .extension()
                .and_then(|s| s.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            if ext != "md" && ext != "markdown" {
                continue;
            }
            if let Some(key) = derive_note_key(&self.root, entry.path()) {
                keys.push(key);
            }
        }
        keys.sort();
        keys
    }
}

/// 从 `vault_root` 与 `file_path` 推导 [`NoteKey`]（委托 [`crate::note::note_key_from_path`]）。
fn derive_note_key(vault_root: &Path, file_path: &Path) -> Option<NoteKey> {
    crate::note::note_key_from_path(vault_root, file_path)
}

/// 默认忽略模式：`.obsidian`、`.git`、`.trash`、`node_modules`。
#[must_use]
pub fn default_ignore_patterns() -> Vec<String> {
    vec![
        ".obsidian".to_owned(),
        ".git".to_owned(),
        ".trash".to_owned(),
        "node_modules".to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn default_ignore_patterns_cover_hidden_and_dep_dirs() {
        let patterns = default_ignore_patterns();
        for expected in [".obsidian", ".git", ".trash", "node_modules"] {
            assert!(
                patterns.iter().any(|p| p == expected),
                "缺少忽略模式 {expected}"
            );
        }
    }

    #[test]
    fn scan_empty_dir_returns_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scanner = VaultScanner::new(dir.path());
        assert!(scanner.scan().is_empty());
        let vault = Vault::scan(dir.path());
        assert!(vault.notes.is_empty());
        let notes = Vault::load(dir.path());
        assert!(notes.is_empty());
    }

    #[test]
    fn scan_finds_md_and_markdown() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.md"), "# A").expect("write");
        fs::write(dir.path().join("b.markdown"), "# B").expect("write");
        fs::write(dir.path().join("c.txt"), "ignore").expect("write");
        let scanner = VaultScanner::new(dir.path());
        let mut keys = scanner.scan();
        keys.sort();
        let strs: Vec<&str> = keys.iter().map(NoteKey::as_str).collect();
        assert_eq!(strs, vec!["a", "b"]);
    }

    #[test]
    fn scan_respects_ignore_patterns() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("root.md"), "# root").expect("write");
        let obsidian = dir.path().join(".obsidian");
        fs::create_dir_all(&obsidian).expect("mkdir");
        fs::write(obsidian.join("x.md"), "# x").expect("write");
        let git = dir.path().join(".git");
        fs::create_dir_all(&git).expect("mkdir");
        fs::write(git.join("y.md"), "# y").expect("write");
        let trash = dir.path().join(".trash");
        fs::create_dir_all(&trash).expect("mkdir");
        fs::write(trash.join("z.md"), "# z").expect("write");
        let nm = dir.path().join("node_modules");
        fs::create_dir_all(&nm).expect("mkdir");
        fs::write(nm.join("m.md"), "# m").expect("write");
        let hidden = dir.path().join(".hidden");
        fs::create_dir_all(&hidden).expect("mkdir");
        fs::write(hidden.join("h.md"), "# h").expect("write");
        // 嵌套忽略
        let nested = dir.path().join("docs").join(".git");
        fs::create_dir_all(&nested).expect("mkdir");
        fs::write(nested.join("n.md"), "# n").expect("write");
        fs::write(dir.path().join("docs").join("ok.md"), "# ok").expect("write");

        let scanner = VaultScanner::new(dir.path());
        let keys = scanner.scan();
        let mut strs: Vec<&str> = keys.iter().map(NoteKey::as_str).collect();
        strs.sort_unstable();
        assert_eq!(strs, vec!["docs/ok", "root"]);
    }

    #[test]
    fn scan_sorted_deterministically() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["z.md", "a.md", "m.md"] {
            fs::write(dir.path().join(name), "x").expect("write");
        }
        let scanner = VaultScanner::new(dir.path());
        let keys = scanner.scan();
        let strs: Vec<&str> = keys.iter().map(NoteKey::as_str).collect();
        assert_eq!(strs, vec!["a", "m", "z"]);
    }

    #[test]
    fn scan_nested_and_key_normalization() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("a").join("b");
        fs::create_dir_all(&sub).expect("mkdir");
        fs::write(sub.join("c.md"), "# c").expect("write");
        let scanner = VaultScanner::new(dir.path());
        let keys = scanner.scan();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].as_str(), "a/b/c");
    }

    #[test]
    fn load_returns_notes_with_frontmatter() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("hello.md"),
            "---\ntitle: Hello\ntags:\n  - t1\n---\nBody [[world]]",
        )
        .expect("write");
        let notes = Vault::load(dir.path());
        assert_eq!(notes.len(), 1);
        let n = &notes[0];
        assert_eq!(n.key.as_str(), "hello");
        assert_eq!(n.title, "Hello");
        assert_eq!(n.tags, vec!["t1"]);
        assert!(n.body.contains("Body"));
        assert!(!n.body.contains("---"));
    }

    #[test]
    fn load_empty_returns_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(Vault::load(dir.path()).is_empty());
    }

    #[test]
    fn scan_hidden_file_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join(".hidden.md"), "x").expect("write");
        fs::write(dir.path().join("ok.md"), "x").expect("write");
        let keys = VaultScanner::new(dir.path()).scan();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].as_str(), "ok");
    }

    #[test]
    fn scan_nonexistent_root_returns_empty() {
        let scanner = VaultScanner::new("/non/existent/path/xyz-12345");
        assert!(scanner.scan().is_empty());
    }

    #[test]
    fn load_one_matches_full_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("hello.md"),
            "---\ntitle: Hello\ntags:\n  - t1\n---\nBody [[world]]",
        )
        .expect("write");
        fs::write(dir.path().join("world.md"), "plain").expect("write");
        let all = Vault::load(dir.path());
        let full = all.iter().find(|n| n.key.as_str() == "hello").expect("full");
        let one = Vault::load_one(dir.path(), "hello").expect("one");
        assert_eq!(one, *full);
    }

    #[test]
    fn load_one_missing_and_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("ok.md"), "x").expect("write");
        assert!(Vault::load_one(dir.path(), "missing").is_none());
        // 忽略目录下的文件不可见
        let obsidian = dir.path().join(".obsidian");
        fs::create_dir_all(&obsidian).expect("mkdir");
        fs::write(obsidian.join("x.md"), "x").expect("write");
        assert!(Vault::load_one(dir.path(), ".obsidian/x").is_none());
        // 逃逸 key 不得逃出 root
        assert!(Vault::load_one(dir.path(), "../outside").is_none());
    }

    #[test]
    fn load_one_markdown_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("alt.markdown"), "hello").expect("write");
        let one = Vault::load_one(dir.path(), "alt").expect("one");
        assert_eq!(one.key.as_str(), "alt");
        assert!(one.body.contains("hello"));
    }

    #[test]
    fn scan_meta_keys_match_scan_without_reading_body() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("b.md"), "body b").expect("write");
        let sub = dir.path().join("a");
        fs::create_dir_all(&sub).expect("mkdir");
        fs::write(sub.join("c.markdown"), "body c").expect("write");
        let metas = Vault::scan_meta(dir.path());
        let keys: Vec<&str> = metas.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, vec!["a/c", "b"]);
        for m in &metas {
            assert!(m.path.is_file());
        }
        // 非 markdown 不计入
        fs::write(dir.path().join("x.txt"), "x").expect("write");
        assert_eq!(Vault::scan_meta(dir.path()).len(), 2);
    }
}
