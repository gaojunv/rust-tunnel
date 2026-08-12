//! Agent workbench REST + WebSocket handlers.
//!
//! 子模块按 handler 聚类拆分：`workspaces`（workspace CRUD + FS/Git 面板）、
//! `sessions`（session CRUD + 默认模型）、`ws`（回合流 + 终端桥接）、`dto`（类型）。
//! 本模块对外（`crate::mgmt::api::agent::xxx`）通过 `pub use` 保持原扁平路径兼容。

mod dto;
mod sessions;
mod workspaces;
mod ws;

pub use dto::*;
pub use sessions::{
    archive_session, create_session, delete_session, get_default_model, list_messages,
    list_sessions, put_default_model, update_session, update_session_model,
};
pub use workspaces::{
    create_workspace, delete_workspace, get_fs_file, get_fs_tree, get_git_diff, get_git_status,
    get_workspace, list_workspace_files, list_workspaces, put_fs_file, update_workspace,
};
pub use ws::{agent_ws, terminal_ws};

/// Generate a random hex id (32 hex chars, 128-bit)。
/// workspaces/sessions 共用（create_workspace / create_session）。
fn new_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}
