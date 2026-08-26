//! Vault 目录扫描：发现 vault 根目录下的全部 Markdown 笔记。
//!
//! 当前为骨架：`VaultScanner::scan` 为桩实现，真实遍历（walkdir、忽略
//! `.obsidian`/`.git`/`.trash`/`node_modules`）在后续批次实现。

use std::path::PathBuf;

use crate::note::NoteKey;

/// 一个已发现的 Markdown vault。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vault {
    /// vault 根目录。
    pub root: PathBuf,
    /// 已发现的笔记键。
    pub notes: Vec<NoteKey>,
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

    /// 扫描 vault，返回全部笔记键。
    ///
    /// 当前为桩实现：恒返回空列表。
    #[must_use]
    pub fn scan(&self) -> Vec<NoteKey> {
        Vec::new()
    }
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
    fn scanner_stub_scan_returns_empty() {
        let scanner = VaultScanner::new(std::env::temp_dir());
        assert!(scanner.scan().is_empty());
    }
}
