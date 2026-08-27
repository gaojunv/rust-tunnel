// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

/*!
桌面笔记应用的 Tauri IPC 后端。

本 crate 把 [`rust_tunnel_wiki_core`] 的纯逻辑包装成 Tauri command 暴露给前端。
领域逻辑位于 [`vault_ops`]（完全不依赖 `tauri`），[`commands`] 为面向
`&AppState` 的纯函数薄包装，[`tauri_app`] 为 `#[tauri::command]` 装配层
（`tauri::State` 解包 + `generate_handler!` / `generate_context!`）；[`dto`]
与 [`error`] 定义 IPC 边界类型，[`state`] 持有 vault 根目录。

# Feature 门控

- `search`（默认开启）：启用 `rust-tunnel-wiki-core/search`，`vault_ops::search_notes`
  走 tantivy 临时索引；关闭时为朴素子串匹配降级。
- `tauri`：启用 `tauri`/`tauri-build` 依赖，`tauri_app` 装配 `Builder::manage` /
  `generate_handler!` / `generate_context!` 真链路（含 icons 校验与二进制链接），
  已在本机通过 `cargo build --features tauri` 验证；不带该 feature 时全部
  Tauri 类型均不参与编译，`cargo test`/`cargo check` 可完全覆盖纯逻辑层。

命名说明：`serve` 是沿用初版目录名，实际定位是 IPC 后端而非 HTTP 服务。
*/

/// 纯函数 `command` 层（`&AppState` 为首参，不依赖 `tauri::State`）。
pub mod commands;
/// IPC 数据传输对象。
pub mod dto;
/// IPC 错误类型与结果别名。
pub mod error;
/// 应用状态（vault 根目录）。
pub mod state;
/// Vault 纯逻辑操作（不依赖 `tauri`）。
pub mod vault_ops;
/// Tauri 装配层（仅 `tauri` feature）。
#[cfg(feature = "tauri")]
pub mod tauri_app;

#[cfg(feature = "tauri")]
pub use tauri_app::run;

/// 重导出领域内核，便于后续 command 层单点引用。
pub use rust_tunnel_wiki_core as core;
