// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! 桌面笔记应用的 Tauri IPC 后端——**当前仅骨架**。
//!
//! 本 crate 的职责是把 [`rust_tunnel_wiki_core`] 的纯逻辑包装成 Tauri command
//! 暴露给前端。本批只建立 crate 使其进入 workspace members，不引入 tauri 依赖
//! 链（tauri 在 Linux 上需 webkit2gtk 等系统库，与本批的纯逻辑验收无关）。
//!
//! 命名说明：`serve` 是沿用初版目录名，实际定位是 IPC 后端而非 HTTP 服务。

/// 重导出领域内核，便于后续 command 层单点引用。
pub use rust_tunnel_wiki_core as core;