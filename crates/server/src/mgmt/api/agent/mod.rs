//! Agent workbench REST + WebSocket handlers.
//!
//! 子模块按 handler 聚类拆分：`workspaces`（workspace CRUD + FS/Git 面板）、
//! `sessions`（session CRUD + 默认模型）、`ws`（回合流 + 终端桥接）、`dto`（类型）。
//! 本模块对外（`crate::mgmt::api::agent::xxx`）通过 `pub use` 保持原扁平路径兼容。

mod dto;
mod github;
#[cfg(feature = "rag")]
pub mod memory;
#[cfg(feature = "rag")]
pub mod skills;
mod sessions;
mod workspaces;
mod ws;

pub use dto::*;
pub use github::{
    cancel_run, dispatch_workflow, get_job_logs, get_repo_info, list_run_jobs, list_workflow_runs,
    list_workflows, rerun_workflow,
};
pub use sessions::{
    archive_session, create_session, delete_session, get_default_model, list_messages,
    list_sessions, put_default_model, update_session, update_session_model,
};
pub use workspaces::{
    create_workspace, delete_workspace, get_fs_file, get_fs_tree, get_git_branches, get_git_diff,
    get_git_log, get_git_show, get_git_stash, get_git_status, get_workspace,
    list_workspace_files, list_workspaces, post_git_branch_delete, post_git_checkout,
    post_git_commit, post_git_pull, post_git_push, post_git_reset, post_git_revert,
    post_git_stage, post_git_stash_apply, post_git_stash_drop, post_git_stash_pop,
    post_git_stash_push, post_git_unstage, put_fs_file, update_workspace,
};
pub use ws::{agent_ws, notifications_ws, terminal_ws};

/// Generate a random hex id (32 hex chars, 128-bit)。
/// workspaces/sessions 共用（create_workspace / create_session）。
fn new_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// 取 LLM 字段加密器（github_token 落库复用同一机制；未初始化 / 未配置主密钥时
/// 为 None → 明文兼容降级，与 provider API Key 一致）。
pub(crate) async fn agent_cipher(
    state: &crate::mgmt::api::ApiState,
) -> Option<crate::llm::crypto::LlmCipher> {
    let guard = state.server_state.proxy_state.llm_state.read().await;
    guard.as_ref().and_then(|l| l.cipher.clone())
}

/// 从 ApiState 取 AI 记忆体运行时；未初始化（非 rag 构建 / 未注入）→ 503。
/// memory.rs 与 skills.rs 共用（上提到本模块避免子模块间循环引用）。
#[cfg(feature = "rag")]
pub(crate) fn mem_runtime(
    state: &crate::mgmt::api::ApiState,
) -> Result<crate::agent::memory::MemoryState, (axum::http::StatusCode, String)> {
    let Some(agent) = &state.server_state.agent_state else {
        return Err((
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "agent workbench not initialized".to_string(),
        ));
    };
    let Some(mem) = &agent.memory else {
        return Err((
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "AI memory runtime not initialized".to_string(),
        ));
    };
    Ok(mem.clone())
}
