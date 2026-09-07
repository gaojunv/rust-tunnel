// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

//! Tauri 装配层：`#[tauri::command]` 薄 wrapper + `run()`.
//!
//! 仅随 `tauri` feature 编译。7 个 wrapper 逐一透传到 [`crate::commands`] 的
//! 纯函数（首参 `&AppState`），`tauri::State` 解引用到 `&AppState` 后直接以
//! `&state` 传入。`run()` 负责首启自举（`ensure_vault_ready`）、`manage` 状态
//! 并装配 `generate_handler!` / `generate_context!`。

use tauri::Manager as _;

use crate::agent::bridge::resolve_app_config_dir;
use crate::agent::AgentManager;
use crate::commands;
use crate::dto::{
    AttachmentDto, DeleteFolderResult, GraphDto, NoteDto, NoteSummary, RenameFolderResult,
    SearchHitDto, VaultInfo,
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

/// 设置笔记的 `ref`（校验后写回 frontmatter）。
///
/// # Errors
///
/// 透传 [`commands::set_note_ref`] 的错误。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn set_note_ref(
    state: tauri::State<'_, AppState>,
    key: String,
    ref_id: String,
) -> IpcResult<NoteDto> {
    commands::set_note_ref(&state, key, ref_id)
}

/// 读取附件。
///
/// # Errors
///
/// 透传 [`commands::read_attachment`] 的错误。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn read_attachment(state: tauri::State<'_, AppState>, rel_path: String) -> IpcResult<Vec<u8>> {
    commands::read_attachment(&state, rel_path)
}

/// 保存附件。
///
/// # Errors
///
/// 透传 [`commands::save_attachment`] 的错误。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn save_attachment(
    state: tauri::State<'_, AppState>,
    note_key: String,
    file_name: String,
    data: Vec<u8>,
) -> IpcResult<AttachmentDto> {
    commands::save_attachment(&state, note_key, file_name, data)
}

/// 应用启动时按设置自动启动 agent（后台线程，避免阻塞 setup）.
///
/// 预检全部通过才调用 [`AgentManager::start`]，任一不满足则静默跳过——
/// `start` 内部失败会累积连续失败计数（满 3 次熔断，需保存设置或显式 stop 清零），
/// 预检可避免「网关地址未配/二进制缺失」这类长期不满足的状态在每次启动应用时
/// 白白消耗熔断计数。预检与 `start` 内部校验存在重复，竞态窗口可忽略。
fn spawn_agent_autostart(
    app: &tauri::App,
    agent_manager: std::sync::Arc<std::sync::Mutex<AgentManager>>,
) {
    let app_config_dir = resolve_app_config_dir(app.handle());
    let app_handle = app.handle().clone();
    std::thread::spawn(move || {
        let settings_path = crate::agent::AgentSettings::default_path(&app_config_dir);
        let settings = match crate::agent::AgentSettings::load(&settings_path) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("agent 自动启动跳过：加载 settings 失败（{err}）");
                return;
            }
        };
        if !settings.enabled {
            return;
        }
        if let Err(reason) = settings.validate() {
            eprintln!("agent 自动启动跳过：{reason}");
            return;
        }
        let override_path = settings
            .codex_path_override
            .as_deref()
            .map(std::path::PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        if crate::agent::resolve::resolve_binary_live(override_path.as_deref(), None).is_none() {
            eprintln!("agent 自动启动跳过：未找到 codex 二进制（sidecar/覆盖/PATH 均未命中）");
            return;
        }
        let sink: std::sync::Arc<dyn crate::agent::AgentEventSink> =
            std::sync::Arc::new(crate::agent::bridge::TauriEventSink(app_handle));
        let mut guard = agent_manager
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 失败仅记日志：错误状态已随 `agent:status` 事件广播，前端面板会展示
        if let Err(err) = guard.start(sink) {
            eprintln!("agent 自动启动失败：{err}");
        }
    });
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
    // vault 根在 setup 闭包中使用，需克隆一份避免借用冲突
    let vault_root_for_agent = root.clone();
    tauri::Builder::default()
        .setup(move |app| {
            let app_config_dir = resolve_app_config_dir(app.handle());
            let agent_manager = std::sync::Arc::new(std::sync::Mutex::new(AgentManager::new(
                app_config_dir,
                vault_root_for_agent,
            )));
            app.manage(agent_manager.clone());
            spawn_agent_autostart(app, agent_manager);
            Ok(())
        })
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            delete_folder,
            delete_note,
            get_graph,
            get_note,
            get_vault_info,
            list_notes,
            list_notes_full,
            read_attachment,
            read_sync_state,
            rename_folder,
            rename_note,
            save_attachment,
            save_note,
            search_notes,
            set_note_ref,
            write_sync_state,
            crate::agent::bridge::agent_get_status,
            crate::agent::bridge::agent_start,
            crate::agent::bridge::agent_stop,
            crate::agent::bridge::agent_detect_binary,
            crate::agent::bridge::agent_request,
            crate::agent::bridge::agent_respond,
            crate::agent::bridge::agent_get_settings,
            crate::agent::bridge::agent_save_settings,
            crate::agent::bridge::agent_open_external
        ])
        .run(tauri::generate_context!())
}
