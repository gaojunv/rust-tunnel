//! Wiki 运行时占位（批 1）：链接解析复用 `db::wiki` 的工具，
//! 后续批 3 注入/检索在此扩展。仅 `rag` feature 编译。

pub use crate::persistence::db::wiki::{normalize_wiki_ref, parse_wiki_links};
