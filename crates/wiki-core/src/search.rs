//! 基于 tantivy 的全文检索索引。
//!
//! 本模块仅在 `search` feature 下编译（对应 Cargo 的 `dep:tantivy` 门控）。
//! 当前为骨架：提供类型定义与可编译桩，真实 schema/写入/查询在后续批次实现。

use tantivy::{Index, IndexReader};
use thiserror::Error;

use crate::note::{Note, NoteKey};

/// 全文检索错误。
#[derive(Debug, Error)]
pub enum SearchError {
    /// 检索能力尚未实现。
    #[error("全文检索尚未实现")]
    NotImplemented,
    /// tantivy 底层错误。
    #[error("tantivy 错误：{0}")]
    Tantivy(#[from] tantivy::TantivyError),
}

/// 可持久化的全文索引。
pub struct SearchIndex {
    /// tantivy 索引句柄。
    pub index: Index,
    /// 索引读取器（并发搜索入口）。
    pub reader: IndexReader,
}

/// 一条搜索结果。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// 命中的笔记键。
    pub note_key: NoteKey,
    /// 命中笔记的标题。
    pub title: String,
    /// 命中片段。
    pub snippet: String,
    /// 相关度分数。
    pub score: f64,
}

impl SearchIndex {
    /// 打开或创建位于 `path` 的索引。
    ///
    /// # Errors
    ///
    /// 当前为桩实现：恒返回 [`SearchError::NotImplemented`]。
    pub fn open(_path: impl AsRef<std::path::Path>) -> Result<Self, SearchError> {
        Err(SearchError::NotImplemented)
    }

    /// 将笔记写入索引。
    ///
    /// # Errors
    ///
    /// 当前为桩实现：恒返回 `Ok(())`，不写入任何数据。
    pub fn add_note(&mut self, _note: &Note) -> Result<(), SearchError> {
        Ok(())
    }

    /// 按 `query` 检索，返回前 `limit` 条命中。
    ///
    /// # Errors
    ///
    /// 当前为桩实现：恒返回空结果。
    pub fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_stub_returns_not_implemented() {
        assert!(matches!(
            SearchIndex::open(std::env::temp_dir()),
            Err(SearchError::NotImplemented)
        ));
    }
}
