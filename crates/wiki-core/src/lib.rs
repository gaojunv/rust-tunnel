// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! 本地 Markdown vault 的领域逻辑——桌面笔记应用（`wiki-serve`）的纯计算内核。
//!
//! 本 crate 刻意不依赖 sqlx、不依赖任何 `rust-tunnel-*` crate：它只做「一堆
//! Markdown 文件 → 结构化笔记 / 链接图 / 全文索引」的纯本地推导，不涉及与
//! server 端 wiki 的同步。同步层待 server 侧统一知识模型
//! （`knowledge_sources` / `knowledge_pages`）落地后单独一批实现。

/// Wiki `ref` 标识的规范化与校验（remote 绑定键，可选）。
pub mod ref_id;

// comrak 0.54 API 探针：只随测试编译，验证 5 个解析行为问题（见该文件头部结论）。
// 临时用途，验证后并入 markdown.rs / link.rs 或删除。
#[cfg(test)]
mod comrak_probe;

pub use ref_id::{RefId, MAX_REF_LEN};