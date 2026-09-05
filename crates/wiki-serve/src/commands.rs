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

use crate::dto::{
    AttachmentDto, DeleteFolderResult, GraphDto, NoteDto, NoteSummary, RenameFolderResult,
    SearchHitDto, VaultInfo,
};
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
pub fn search_notes(state: &AppState, query: String, limit: usize) -> IpcResult<Vec<SearchHitDto>> {
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

/// 重命名单篇笔记。
///
/// # Errors
///
/// 路径非法、源不存在或目标已存在时返回 [`crate::error::IpcError`]。
#[allow(clippy::needless_pass_by_value)]
pub fn rename_note(
    state: &AppState,
    key: String,
    new_key: String,
    rewrite_links: bool,
) -> IpcResult<NoteDto> {
    let root = state.root();
    vault_ops::rename_note(&root, &key, &new_key, rewrite_links)
}

/// 重命名文件夹（批量移动）.
///
/// # Errors
///
/// 路径非法、未命中或新前缀为旧前缀子路径时返回 [`crate::error::IpcError`]。
#[allow(clippy::needless_pass_by_value)]
pub fn rename_folder(
    state: &AppState,
    old_prefix: String,
    new_prefix: String,
    rewrite_links: bool,
) -> IpcResult<RenameFolderResult> {
    let root = state.root();
    vault_ops::rename_folder(&root, &old_prefix, &new_prefix, rewrite_links)
}

/// 删除文件夹下的全部笔记.
///
/// # Errors
///
/// 路径非法时返回 [`crate::error::IpcError`]。
#[allow(clippy::needless_pass_by_value)]
pub fn delete_folder(state: &AppState, prefix: String) -> IpcResult<DeleteFolderResult> {
    let root = state.root();
    vault_ops::delete_folder(&root, &prefix)
}

/// 读取同步状态（`<root>/.wiki-sync.json`，不存在返回 `None`）。
///
/// # Errors
///
/// 透传 [`vault_ops::read_sync_state`] 的错误。
pub fn read_sync_state(state: &AppState) -> IpcResult<Option<String>> {
    let root = state.root();
    vault_ops::read_sync_state(&root)
}

/// 原子写入同步状态（`<root>/.wiki-sync.json`）。
///
/// `json` 按 Tauri IPC 约定以拥有字符串传入。
///
/// # Errors
///
/// 透传 [`vault_ops::write_sync_state`] 的错误。
#[allow(clippy::needless_pass_by_value)]
pub fn write_sync_state(state: &AppState, json: String) -> IpcResult<()> {
    let root = state.root();
    vault_ops::write_sync_state(&root, &json)
}

/// 一次拿全量笔记（含 `body`/`ref_id`），避免 N+1 次 `get_note`。
///
/// # Errors
///
/// 透传 [`vault_ops::list_notes_full`] 的错误。
pub fn list_notes_full(state: &AppState) -> IpcResult<Vec<NoteDto>> {
    let root = state.root();
    vault_ops::list_notes_full(&root)
}

/// 设置笔记的 `ref`（校验后写回 frontmatter）。
///
/// `key`/`ref_id` 按 Tauri IPC 约定以拥有字符串传入，见 [`get_note`]。
///
/// # Errors
///
/// - `ref_id` 非法时返回 [`crate::error::IpcError::InvalidArgument`]
/// - 路径逃逸 / 笔记不存在时透传 [`vault_ops::set_note_ref`] 的错误
#[allow(clippy::needless_pass_by_value)]
pub fn set_note_ref(state: &AppState, key: String, ref_id: String) -> IpcResult<NoteDto> {
    let root = state.root();
    vault_ops::set_note_ref(&root, &key, &ref_id)
}

/// 保存附件（`data` 为字节数组，透传到 `vault_ops`）。
///
/// # Errors
///
/// 透传 [`vault_ops::save_attachment`] 的错误。
#[allow(clippy::needless_pass_by_value)]
pub fn save_attachment(
    state: &AppState,
    note_key: String,
    file_name: String,
    data: Vec<u8>,
) -> IpcResult<AttachmentDto> {
    let root = state.root();
    vault_ops::save_attachment(&root, &note_key, &file_name, &data)
}

/// 读取附件。
///
/// # Errors
///
/// 透传 [`vault_ops::read_attachment`] 的错误。
#[allow(clippy::needless_pass_by_value)]
pub fn read_attachment(state: &AppState, rel_path: String) -> IpcResult<Vec<u8>> {
    let root = state.root();
    vault_ops::read_attachment(&root, &rel_path)
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

        // 新增同步命令同样走透传路径（照抄现有范式）
        let full = list_notes_full(&state).expect("full");
        assert_eq!(full.len(), 1);
        assert!(full[0].body.contains("body"));

        assert!(read_sync_state(&state).expect("read none").is_none());
        write_sync_state(&state, r#"{"v":1}"#.to_owned()).expect("write sync");
        let back = read_sync_state(&state).expect("read back").expect("some");
        assert_eq!(back, r#"{"v":1}"#);

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

    #[test]
    fn commands_sync_state_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = AppState::new(dir.path().to_path_buf());
        assert!(read_sync_state(&state).expect("none").is_none());
        write_sync_state(&state, r#"{"a":1}"#.to_owned()).expect("write");
        let got = read_sync_state(&state).expect("read").expect("some");
        assert_eq!(got, r#"{"a":1}"#);
        // 覆盖写入
        write_sync_state(&state, r#"{"b":2}"#.to_owned()).expect("write2");
        let got2 = read_sync_state(&state).expect("read2").expect("some2");
        assert_eq!(got2, r#"{"b":2}"#);
    }

    #[test]
    fn commands_list_notes_full_has_body_and_ref() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = AppState::new(dir.path().to_path_buf());
        save_note(
            &state,
            "a".to_owned(),
            "---\nref: a/b\ntitle: T\n---\nhello body".to_owned(),
            None,
        )
        .expect("save");
        save_note(&state, "b".to_owned(), "plain body".to_owned(), None).expect("save b");
        let full = list_notes_full(&state).expect("full");
        assert_eq!(full.len(), 2);
        let a = full.iter().find(|n| n.key == "a").expect("a");
        assert_eq!(a.ref_id.as_deref(), Some("a/b"));
        assert!(a.body.contains("hello body"));
        let b = full.iter().find(|n| n.key == "b").expect("b");
        assert!(b.ref_id.is_none());
    }
}
