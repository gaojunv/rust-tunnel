// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

/*!
薄 `command` 包装层（纯函数，不依赖 `tauri`）。

# 分层理由

Tauri 的 `tauri::State<'_, T>` 只在 `tauri` feature 开启时存在，且
`#[tauri::command]` 要求参数满足 `CommandArg`（`&AppState` 不满足）；若
直接把 `#[tauri::command]` 放在本文件，`generate_handler!` 展开时才会触发
参数解析实例化而编译失败。

因此本文件的 7 个函数统一以 `&AppState` 为首参（完全不依赖 `tauri` 类型），
是可被 `cargo test` 直接调用的普通函数；真正的 `#[tauri::command]` 薄
`wrapper(state: tauri::State<'_, AppState>, …) { commands::xxx(&state, …) }`
位于 `tauri_app` 装配层，避免逻辑与框架类型耦合。

备选形态「整个 `commands.rs`  `#[cfg(feature = "tauri")]`、逻辑全在
`vault_ops`」也能通过编译，但会使 `command` 层的参数转换（如 `String`→`&str`、`Option<String>` 透传）
无处可测；当前形态保留了这一层的可测性。
*/

use crate::dto::{GraphDto, NoteDto, NoteSummary, SearchHitDto, VaultInfo};
use crate::error::IpcResult;
use crate::state::AppState;
use crate::vault_ops;

/// 获取 vault 信息.
///
/// # Errors
///
/// `root` 不存在或扫描失败时返回 [`crate::error::IpcError`]。
pub fn get_vault_info(state: &AppState) -> IpcResult<VaultInfo> {
    let root = state.root();
    vault_ops::get_vault_info(&root)
}

/// 列出全部笔记摘要.
///
/// # Errors
///
/// `root` 不存在时返回 [`crate::error::IpcError::VaultNotFound`]。
pub fn list_notes(state: &AppState) -> IpcResult<Vec<NoteSummary>> {
    let root = state.root();
    vault_ops::list_notes(&root)
}

/// 读取单篇笔记。
///
/// `key` 按 Tauri IPC 约定以拥有字符串传入（JS 侧 `invoke` 参数为 `string`），
/// 本层仅做透传到 `vault_ops`。
///
/// # Errors
///
/// 路径逃逸或笔记不存在时返回 [`crate::error::IpcError`]。
#[allow(clippy::needless_pass_by_value)]
pub fn get_note(state: &AppState, key: String) -> IpcResult<NoteDto> {
    let root = state.root();
    vault_ops::get_note(&root, &key)
}

/// 保存笔记（创建或覆盖），可选地注入 `title` 到 frontmatter.
///
/// `key`/`body` 按 Tauri IPC 约定以拥有字符串传入，见 [`get_note`]。
///
/// # Errors
///
/// 路径逃逸或 IO 失败时返回 [`crate::error::IpcError`]。
#[allow(clippy::needless_pass_by_value)]
pub fn save_note(
    state: &AppState,
    key: String,
    body: String,
    title: Option<String>,
) -> IpcResult<NoteDto> {
    let root = state.root();
    vault_ops::save_note(&root, &key, &body, title)
}

/// 删除笔记。
///
/// # Errors
///
/// 路径逃逸或笔记不存在时返回 [`crate::error::IpcError`]。
#[allow(clippy::needless_pass_by_value)]
pub fn delete_note(state: &AppState, key: String) -> IpcResult<()> {
    let root = state.root();
    vault_ops::delete_note(&root, &key)
}

/// 搜索笔记。
///
/// `query` 按 Tauri IPC 约定以拥有字符串传入。
///
/// # Errors
///
/// `root` 不存在或检索失败时返回 [`crate::error::IpcError`]。
#[allow(clippy::needless_pass_by_value)]
pub fn search_notes(
    state: &AppState,
    query: String,
    limit: usize,
) -> IpcResult<Vec<SearchHitDto>> {
    let root = state.root();
    vault_ops::search_notes(&root, &query, limit)
}

/// 获取链接图.
///
/// # Errors
///
/// `root` 不存在时返回 [`crate::error::IpcError::VaultNotFound`]。
pub fn get_graph(state: &AppState) -> IpcResult<GraphDto> {
    let root = state.root();
    vault_ops::get_graph(&root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_delegate_to_vault_ops() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = AppState::new(dir.path().to_path_buf());

        let info = get_vault_info(&state).expect("info");
        assert_eq!(info.note_count, 0);

        let dto = save_note(&state, "hello".to_owned(), "body".to_owned(), None).expect("save");
        assert_eq!(dto.key, "hello");

        let fetched = get_note(&state, "hello".to_owned()).expect("get");
        assert_eq!(fetched.key, "hello");

        let list = list_notes(&state).expect("list");
        assert_eq!(list.len(), 1);

        let hits = search_notes(&state, "body".to_owned(), 10).expect("search");
        assert!(!hits.is_empty());

        let graph = get_graph(&state).expect("graph");
        assert_eq!(graph.nodes.len(), 1);

        delete_note(&state, "hello".to_owned()).expect("delete");
        let list2 = list_notes(&state).expect("list2");
        assert!(list2.is_empty());

        assert!(!dir.path().join("hello.md").exists());
        let err = get_note(&state, "missing".to_owned()).expect_err("应不存在");
        assert!(matches!(err, crate::error::IpcError::NoteNotFound(_)));
    }

    #[test]
    fn commands_reject_traversal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = AppState::new(dir.path().to_path_buf());
        let res = get_note(&state, "../etc/passwd".to_owned());
        assert!(res.is_err());
        let res2 = save_note(&state, "../../bad".to_owned(), "x".to_owned(), None);
        assert!(res2.is_err());
    }
}
