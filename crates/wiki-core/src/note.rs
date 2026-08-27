//! 笔记领域模型：`NoteKey`（vault 内相对路径键）与 `Note`。
//!
//! `NoteKey` 是本 crate 各模块（链接图、检索、扫描）共享的笔记标识，以 vault
//! 根目录为基准的相对路径（去扩展名、`\` 归一为 `/`）表示。

use std::fmt;
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::frontmatter::FrontMatter;
use crate::link::WikiLink;
use crate::ref_id::RefId;

/// 笔记唯一键：vault 内相对路径，去扩展名，`\` 已归一为 `/`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NoteKey(String);

impl NoteKey {
    /// 由原始字符串构造 `NoteKey`（调用方需保证已规范化）。
    #[must_use]
    pub fn new(s: String) -> Self {
        Self(s)
    }

    /// 内部字符串的只读视图。
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 取出内部字符串。
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for NoteKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for NoteKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// 一篇已加载的笔记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// 笔记唯一键。
    pub key: NoteKey,
    /// 绑定到 server 的 remote `ref`（可选）。
    pub ref_id: Option<RefId>,
    /// 显示标题。
    pub title: String,
    /// 别名列表。
    pub aliases: Vec<String>,
    /// 标签列表。
    pub tags: Vec<String>,
    /// 正文（已剥离 frontmatter 的原始 Markdown）。
    pub body: String,
    /// 正文中提取的 wiki 链接。
    pub wikilinks: Vec<WikiLink>,
    /// 笔记头部解析出的 frontmatter。
    pub frontmatter: FrontMatter,
    /// 最后修改时间。
    pub modified: SystemTime,
}

/// 从 `vault_root` 与 `file_path` 推导 [`NoteKey`]。
///
/// 基于 `vault_root` 计算相对路径，去 `.md`/`.markdown` 扩展（大小写不敏感、按
/// 字节长度切除避免大小写不一致时的切片错位），并将 `\` 归一为 `/`。非 Markdown
/// 文件或 `file_path` 不在 `vault_root` 之下时返回 `None`。
#[must_use]
pub fn note_key_from_path(vault_root: &Path, file_path: &Path) -> Option<NoteKey> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_key_roundtrips_string() {
        let key = NoteKey::new("a/b".to_owned());
        assert_eq!(key.as_str(), "a/b");
        assert_eq!(key.clone().into_string(), "a/b");
        assert_eq!(key.to_string(), "a/b");
    }

    #[test]
    fn note_key_from_path_nested_dir() {
        assert_eq!(
            note_key_from_path(Path::new("/vault"), Path::new("/vault/a/b/c.md"))
                .as_ref()
                .map(NoteKey::as_str),
            Some("a/b/c")
        );
    }

    #[test]
    fn note_key_from_path_markdown_extension() {
        assert_eq!(
            note_key_from_path(Path::new("/vault"), Path::new("/vault/hello.markdown"))
                .as_ref()
                .map(NoteKey::as_str),
            Some("hello")
        );
    }

    #[test]
    fn note_key_from_path_uppercase_md_extension() {
        assert_eq!(
            note_key_from_path(Path::new("/vault"), Path::new("/vault/hello.MD"))
                .as_ref()
                .map(NoteKey::as_str),
            Some("hello")
        );
        assert_eq!(
            note_key_from_path(Path::new("/vault"), Path::new("/vault/hello.MARKDOWN"))
                .as_ref()
                .map(NoteKey::as_str),
            Some("hello")
        );
    }

    #[test]
    fn note_key_from_path_non_md_returns_none() {
        assert_eq!(
            note_key_from_path(Path::new("/vault"), Path::new("/vault/a.txt")),
            None
        );
        assert_eq!(
            note_key_from_path(Path::new("/vault"), Path::new("/vault/a")),
            None
        );
    }

    #[test]
    fn note_key_from_path_outside_root_returns_none() {
        assert_eq!(
            note_key_from_path(Path::new("/vault"), Path::new("/other/a.md")),
            None
        );
        assert_eq!(
            note_key_from_path(Path::new("/vault"), Path::new("/vault-other/a.md")),
            None
        );
    }

    #[test]
    fn note_key_from_path_windows_backslash() {
        assert_eq!(
            note_key_from_path(Path::new("/vault"), Path::new("/vault/a\\b\\c.md"))
                .as_ref()
                .map(NoteKey::as_str),
            Some("a/b/c")
        );
    }
}
