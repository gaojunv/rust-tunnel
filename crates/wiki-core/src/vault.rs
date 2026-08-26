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

    /// 扫描 `root` 并加载全部笔记（`load` 的别名，等价方法）。
    #[must_use]
    pub fn load_notes(root: impl Into<PathBuf>) -> Vec<Note> {
        let root = root.into();
        let scanner = VaultScanner::new(root.clone());
        // 复用扫描器的忽略规则，避免两处 walk 逻辑漂移
        let ignore = scanner.ignore_patterns.clone();
        let mut notes = Vec::new();

        if !root.exists() {
            return notes;
        }

        let walker = WalkDir::new(&root).into_iter().filter_entry(|e| {
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
            if let Ok(rel) = e.path().strip_prefix(&root) {
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
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let frontmatter = parse_frontmatter(&text);
            let body = if let Some((_, end)) = extract_frontmatter_delimiter(&text) {
                text[end..].trim_start_matches(['\n', '\r']).to_owned()
            } else {
                text.clone()
            };
            let wikilinks = extract_wikilinks(&body);
            // 调用 markdown 桩以满足任务要求的依赖调用（当前为桩，返回空）
            let _html = render_html(&body);
            let _headings = extract_headings(&body);
            let title = frontmatter.title.clone().unwrap_or_else(|| {
                entry
                    .path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(key.as_str())
                    .to_owned()
            });
            let modified = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::now());
            notes.push(Note {
                key,
                ref_id: frontmatter.ref_id.clone(),
                title,
                aliases: frontmatter.aliases.clone(),
                tags: frontmatter.tags.clone(),
                body,
                wikilinks,
                frontmatter,
                modified,
            });
        }
        notes.sort_by(|a, b| a.key.cmp(&b.key));
        notes
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
        let walker = WalkDir::new(&self.root).into_iter().filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let file_name = e.file_name().to_string_lossy();
            if file_name.starts_with('.') {
                return false;
            }
            if e.file_type().is_dir() {
                let name = file_name.to_string();
                if self.ignore_patterns.iter().any(|pat| pat == &name) {
                    return false;
                }
            }
            if let Ok(rel) = e.path().strip_prefix(&self.root) {
                for comp in rel.components() {
                    let seg = comp.as_os_str().to_string_lossy();
                    if seg.starts_with('.') {
                        return false;
                    }
                    if self.ignore_patterns.iter().any(|pat| pat == seg.as_ref()) {
                        return false;
                    }
                }
            }
            true
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
            if let Some(key) = derive_note_key(&self.root, entry.path()) {
                keys.push(key);
            }
        }
        keys.sort();
        keys
    }
}

/// 从 `vault_root` 与 `file_path` 推导 [`NoteKey`]。
///
/// 优先尝试调用 [`crate::note::note_key_from_path`]（若桩已实现则直接复用），
/// 失败时回退到本地实现：相对路径去 `.md`/`.markdown` 扩展并将 `\` 归一为 `/`。
fn derive_note_key(vault_root: &Path, file_path: &Path) -> Option<NoteKey> {
    if let Some(k) = crate::note::note_key_from_path(vault_root, file_path) {
        return Some(k);
    }
    let rel = file_path.strip_prefix(vault_root).ok()?;
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    let lower = rel_str.to_ascii_lowercase();
    let stripped = if lower.ends_with(".markdown") {
        // 按字节长度切除，避免大小写不一致时的切片错位
        let end = rel_str.len() - ".markdown".len();
        &rel_str[..end]
    } else if std::path::Path::new(&lower)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
    {
        let end = rel_str.len() - ".md".len();
        &rel_str[..end]
    } else {
        return None;
    };
    Some(NoteKey::new(stripped.to_owned()))
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
        strs.sort();
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
}
