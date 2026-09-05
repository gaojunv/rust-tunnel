import type { DeleteFolderResult, GraphDto, NoteDto, NoteSummary, RenameFolderResult, SearchHitDto, VaultInfo } from "./types";
import { mockVault } from "./mock";

// 是否处于 Tauri 容器内
export const isTauri =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

// 通用 invoke 封装：Tauri 环境动态导入并 invoke，否则走 mock
async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke<T>(cmd, args);
  }
  // —— mock 分支 —— //
  switch (cmd) {
    case "get_vault_info":
      return mockVault.getVaultInfo() as unknown as T;
    case "list_notes":
      return mockVault.listNotes() as unknown as T;
    case "get_note":
      return mockVault.getNote(args?.["key"] as string) as unknown as T;
    case "save_note":
      return mockVault.saveNote(
        args?.["key"] as string,
        args?.["body"] as string,
        args?.["title"] as string | undefined,
      ) as unknown as T;
    case "delete_note":
      return mockVault.deleteNote(args?.["key"] as string) as unknown as T;
    case "search_notes":
      return mockVault.searchNotes(
        args?.["query"] as string,
        args?.["limit"] as number,
      ) as unknown as T;
    case "get_graph":
      return mockVault.getGraph() as unknown as T;
    case "rename_note":
      return mockVault.renameNote(
        args?.["key"] as string,
        args?.["newKey"] as string,
        args?.["rewriteLinks"] as boolean,
      ) as unknown as T;
    case "rename_folder":
      return mockVault.renameFolder(
        args?.["oldPrefix"] as string,
        args?.["newPrefix"] as string,
        args?.["rewriteLinks"] as boolean,
      ) as unknown as T;
    case "delete_folder":
      return mockVault.deleteFolder(args?.["prefix"] as string) as unknown as T;
    case "list_notes_full":
      return mockVault.listNotesFull() as unknown as T;
    case "read_sync_state":
      return mockVault.readSyncState() as unknown as T;
    case "write_sync_state":
      return mockVault.writeSyncState(args?.["json"] as string) as unknown as T;
    default:
      throw new Error(`未知命令: ${cmd}`);
  }
}

// 具名 API（命令名与 Rust 侧一一对应，参数名用 camelCase，Tauri 2 自动映射到 snake_case）
//
// 返回类型严格对齐 Rust 的 IpcResult<T>：
//   - Rust 的 Err(_) 会让 invoke 返回的 Promise **reject**，不会解析成 null，
//     因此 getNote 的返回类型是 NoteDto 而非 NoteDto | null——「笔记不存在」
//     必须在 catch 里处理。
//   - Rust 的 Ok(()) 序列化为 null，故 deleteNote 是 Promise<void>。
//   - search_notes 的 limit 在 Rust 侧是必填 usize，此处不可选。

export function vaultInfo(): Promise<VaultInfo> {
  return call<VaultInfo>("get_vault_info");
}

export function listNotes(): Promise<NoteSummary[]> {
  return call<NoteSummary[]>("list_notes");
}

export function getNote(key: string): Promise<NoteDto> {
  return call<NoteDto>("get_note", { key });
}

export function saveNote(key: string, body: string, title?: string): Promise<NoteDto> {
  return call<NoteDto>("save_note", { key, body, title });
}

export function deleteNote(key: string): Promise<void> {
  return call<void>("delete_note", { key });
}

export function searchNotes(query: string, limit: number): Promise<SearchHitDto[]> {
  return call<SearchHitDto[]>("search_notes", { query, limit });
}

export function getGraph(): Promise<GraphDto> {
  return call<GraphDto>("get_graph");
}

export function renameNote(key: string, newKey: string, rewriteLinks: boolean): Promise<NoteDto> {
  return call<NoteDto>("rename_note", { key, newKey, rewriteLinks });
}

export function renameFolder(
  oldPrefix: string,
  newPrefix: string,
  rewriteLinks: boolean,
): Promise<RenameFolderResult> {
  return call<RenameFolderResult>("rename_folder", { oldPrefix, newPrefix, rewriteLinks });
}

export function deleteFolder(prefix: string): Promise<DeleteFolderResult> {
  return call<DeleteFolderResult>("delete_folder", { prefix });
}

export function listNotesFull(): Promise<NoteDto[]> {
  return call<NoteDto[]>("list_notes_full");
}

export function readSyncState(): Promise<string | null> {
  return call<string | null>("read_sync_state");
}

export function writeSyncState(json: string): Promise<void> {
  return call<void>("write_sync_state", { json });
}
