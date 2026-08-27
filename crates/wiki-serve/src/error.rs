// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

//! IPC 错误类型（`Serialize` 为字符串，供 Tauri command 的 `Err` 使用）。

use thiserror::Error;

/// IPC 操作的错误。
#[derive(Debug, Error)]
pub enum IpcError {
    /// vault 根目录不存在或不可访问。
    #[error("vault 路径不存在：{0}")]
    VaultNotFound(String),
    /// 笔记不存在。
    #[error("笔记不存在：{0}")]
    NoteNotFound(String),
    /// 路径逃逸（`..`、绝对路径或空串）。
    #[error("非法路径：{0}")]
    PathTraversal(String),
    /// 非法参数。
    #[error("非法参数：{0}")]
    InvalidArgument(String),
    /// IO 错误。
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    /// 全文检索错误（仅 `search` feature）。
    #[cfg(feature = "search")]
    #[error("检索错误：{0}")]
    Search(#[from] rust_tunnel_wiki_core::search::SearchError),
    /// 检索错误（无 `search` feature 时的字符串形态，占位以保持字段命名一致）。
    #[cfg(not(feature = "search"))]
    #[error("检索错误：{0}")]
    Search(String),
}

impl serde::Serialize for IpcError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// IPC 结果别名。
pub type IpcResult<T> = Result<T, IpcError>;
