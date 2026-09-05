// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

//! Tauri 装配层：`#[tauri::command]` 薄 wrapper + `run()`.
//!
//! 仅随 `tauri` feature 编译。7 个 wrapper 逐一透传到 [`crate::commands`] 的
//! 纯函数（首参 `&AppState`），`tauri::State` 解引用到 `&AppState` 后直接以
//! `&state` 传入。`run()` 负责首启自举（`ensure_vault_ready`）、`manage` 状态
//! 并装配 `generate_handler!` / `generate_context!`。

use crate::commands;
use crate::dto::{
    DeleteFolderResult, GraphDto, NoteDto, NoteSummary, RenameFolderResult, SearchHitDto, VaultInfo,
};
use crate::error::IpcResult;
use crate::state::AppState;

/// 获取 vault 信息.
///
/// # Errors
///
/// 透传 [`commands::get_vault_info`] 的错误（`root` 不存在等）。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_vault_info(state: tauri::State<'_, AppState>) -> IpcResult<VaultInfo> {
    commands::get_vault_info(&state)
}

/// 列出全部笔记摘要.
///
/// # Errors
///
/// 透传 [`commands::list_notes`] 的错误（`root` 不存在时 `VaultNotFound`）。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn list_notes(state: tauri::State<'_, AppState>) -> IpcResult<Vec<NoteSummary>> {
    commands::list_notes(&state)
}

/// 读取单篇笔记.
///
/// # Errors
///
/// 透传 [`commands::get_note`] 的错误（路径逃逸、笔记不存在等）。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_note(state: tauri::State<'_, AppState>, key: String) -> IpcResult<NoteDto> {
    commands::get_note(&state, key)
}

/// 保存笔记（创建或覆盖）.
///
/// # Errors
///
/// 透传 [`commands::save_note`] 的错误（路径逃逸、IO 失败等）。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn save_note(
    state: tauri::State<'_, AppState>,
    key: String,
    body: String,
    title: Option<String>,
) -> IpcResult<NoteDto> {
    commands::save_note(&state, key, body, title)
}

/// 删除笔记.
///
/// # Errors
///
/// 透传 [`commands::delete_note`] 的错误（路径逃逸、笔记不存在等）。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn delete_note(state: tauri::State<'_, AppState>, key: String) -> IpcResult<()> {
    commands::delete_note(&state, key)
}

/// 搜索笔记.
///
/// # Errors
///
/// 透传 [`commands::search_notes`] 的错误（`root` 不存在或检索失败等）。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn search_notes(
    state: tauri::State<'_, AppState>,
    query: String,
    limit: usize,
) -> IpcResult<Vec<SearchHitDto>> {
    commands::search_notes(&state, query, limit)
}

/// 获取链接图.
///
/// # Errors
///
/// 透传 [`commands::get_graph`] 的错误（`root` 不存在时 `VaultNotFound`）。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_graph(state: tauri::State<'_, AppState>) -> IpcResult<GraphDto> {
    commands::get_graph(&state)
}

/// 重命名单篇笔记.
///
/// # Errors
///
/// 透传 [`commands::rename_note`] 的错误。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn rename_note(
    state: tauri::State<'_, AppState>,
    key: String,
    new_key: String,
    rewrite_links: bool,
) -> IpcResult<NoteDto> {
    commands::rename_note(&state, key, new_key, rewrite_links)
}

/// 重命名文件夹.
///
/// # Errors
///
/// 透传 [`commands::rename_folder`] 的错误。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn rename_folder(
    state: tauri::State<'_, AppState>,
    old_prefix: String,
    new_prefix: String,
    rewrite_links: bool,
) -> IpcResult<RenameFolderResult> {
    commands::rename_folder(&state, old_prefix, new_prefix, rewrite_links)
}

/// 删除文件夹.
///
/// # Errors
///
/// 透传 [`commands::delete_folder`] 的错误。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn delete_folder(
    state: tauri::State<'_, AppState>,
    prefix: String,
) -> IpcResult<DeleteFolderResult> {
    commands::delete_folder(&state, prefix)
}

/// 读取同步状态（`<root>/.wiki-sync.json`，不存在返回 `null`）。
///
/// # Errors
///
/// 透传 [`commands::read_sync_state`] 的错误。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn read_sync_state(state: tauri::State<'_, AppState>) -> IpcResult<Option<String>> {
    commands::read_sync_state(&state)
}

/// 原子写入同步状态（`<root>/.wiki-sync.json`）。
///
/// # Errors
///
/// 透传 [`commands::write_sync_state`] 的错误。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn write_sync_state(state: tauri::State<'_, AppState>, json: String) -> IpcResult<()> {
    commands::write_sync_state(&state, json)
}

/// 一次拿全量笔记（含 `body`/`ref_id`）。
///
/// # Errors
///
/// 透传 [`commands::list_notes_full`] 的错误。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn list_notes_full(state: tauri::State<'_, AppState>) -> IpcResult<Vec<NoteDto>> {
    commands::list_notes_full(&state)
}

/// 启动 Tauri 应用.
///
/// 首启时确保 vault 根目录存在且非空（空 vault 写入 `welcome.md`），随后
/// `manage` 状态并以 `generate_context!` / `generate_handler!` 启动。
///
/// # Errors
///
/// `tauri::Builder::run` 失败时返回 [`tauri::Error`]。
pub fn run() -> tauri::Result<()> {
    let state = AppState::from_env_or_default();
    let root = state.root();
    if let Err(err) = crate::vault_ops::ensure_vault_ready(&root) {
        eprintln!("failed to ensure vault ready at {}: {err}", root.display());
    }
    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_vault_info,
            list_notes,
            get_note,
            save_note,
            delete_note,
            search_notes,
            get_graph,
            rename_note,
            rename_folder,
            delete_folder,
            read_sync_state,
            write_sync_state,
            list_notes_full
        ])
        .run(tauri::generate_context!())
}
