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

/// 从绝对文件路径推导 `NoteKey`。
///
/// 当前为桩实现：恒返回 `None`。真实实现会基于 `vault_root` 计算相对路径、
/// 去扩展名并将 `\` 归一为 `/`。
#[must_use]
pub fn note_key_from_path(_vault_root: &Path, _file_path: &Path) -> Option<NoteKey> {
    None
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
    fn path_stub_returns_none() {
        assert_eq!(
            note_key_from_path(Path::new("/vault"), Path::new("/vault/a.md")),
            None
        );
    }
}
