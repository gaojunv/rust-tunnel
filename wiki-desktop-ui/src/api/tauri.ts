import type { DeleteFolderResult, GraphDto, NoteDto, NoteSummary, RenameFolderResult, SearchHitDto, VaultInfo } from "./types";

// mockVault 仅非 Tauri 环境（浏览器/mock）使用：动态导入，生产主包不含 mock.ts。
// （Tauri 容器内 mock 分支永不执行，mock chunk 不会被加载。）
type MockVault = typeof import("./mock").mockVault;
let mockVaultPromise: Promise<MockVault> | null = null;
function loadMockVault(): Promise<MockVault> {
  mockVaultPromise ??= import("./mock").then((m) => m.mockVault);
  return mockVaultPromise;
}

// 是否处于 Tauri 容器内
export const isTauri =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

// —— base64 编解码（附件 IPC 用字符串传输，避免 number[] JSON 膨胀 3~4 倍）——
// btoa 入参是 UTF-16 码元串：按 32KB 分块 fromCharCode，避免大文件栈溢出。

const B64_CHUNK = 0x8000;

export function uint8ToBase64(bytes: Uint8Array): string {
  let s = "";
  for (let i = 0; i < bytes.length; i += B64_CHUNK) {
    s += String.fromCharCode(...bytes.subarray(i, i + B64_CHUNK));
  }
  return btoa(s);
}

export function base64ToBytes(b64: string): Uint8Array {
  const s = atob(b64);
  const out = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
  return out;
}

// 通用 invoke 封装：Tauri 环境动态导入并 invoke，否则走 mock
async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke<T>(cmd, args);
  }
  // —— mock 分支 —— //
  const mv = await loadMockVault();
  switch (cmd) {
    case "get_vault_info":
      return mv.getVaultInfo() as unknown as T;
    case "list_notes":
      return mv.listNotes() as unknown as T;
    case "get_note":
      return mv.getNote(args?.["key"] as string) as unknown as T;
    case "save_note":
      return mv.saveNote(
        args?.["key"] as string,
        args?.["body"] as string,
        args?.["title"] as string | undefined,
      ) as unknown as T;
    case "delete_note":
      return mv.deleteNote(args?.["key"] as string) as unknown as T;
    case "search_notes":
      return mv.searchNotes(
        args?.["query"] as string,
        args?.["limit"] as number,
      ) as unknown as T;
    case "get_graph":
      return mv.getGraph() as unknown as T;
    case "rename_note":
      return mv.renameNote(
        args?.["key"] as string,
        args?.["newKey"] as string,
        args?.["rewriteLinks"] as boolean,
      ) as unknown as T;
    case "rename_folder":
      return mv.renameFolder(
        args?.["oldPrefix"] as string,
        args?.["newPrefix"] as string,
        args?.["rewriteLinks"] as boolean,
      ) as unknown as T;
    case "delete_folder":
      return mv.deleteFolder(args?.["prefix"] as string) as unknown as T;
    case "list_notes_full":
      return mv.listNotesFull() as unknown as T;
    case "read_sync_state":
      return mv.readSyncState() as unknown as T;
    case "write_sync_state":
      return mv.writeSyncState(args?.["json"] as string) as unknown as T;
    case "set_note_ref":
      return mv.setNoteRef(args?.["key"] as string, args?.["refId"] as string) as unknown as T;
    case "save_attachment": {
      const data = args?.["data"] as string | undefined;
      const bytes = data ? base64ToBytes(data) : new Uint8Array();
      return mv.saveAttachment(
        args?.["noteKey"] as string,
        args?.["fileName"] as string,
        bytes,
      ) as unknown as T;
    }
    case "read_attachment": {
      const raw = mv.readAttachment(args?.["relPath"] as string) as unknown as Uint8Array;
      return uint8ToBase64(raw) as unknown as T;
    }
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

export function setNoteRef(key: string, ref: string): Promise<NoteDto> {
  return call<NoteDto>("set_note_ref", { key, refId: ref });
}

export function saveAttachment(
  noteKey: string,
  fileName: string,
  data: Uint8Array,
): Promise<{ rel_path: string }> {
  return call<{ rel_path: string }>("save_attachment", {
    noteKey,
    fileName,
    data: uint8ToBase64(data),
  });
}

export function readAttachment(relPath: string): Promise<Uint8Array> {
  return call<string>("read_attachment", { relPath }).then((b64) => base64ToBytes(b64));
}

// —— vault 外部文件变更订阅 ——
//
// Rust 侧 `spawn_vault_watcher`（crates/wiki-serve/src/tauri_app.rs）递归监听
// vault 根目录，debounce ~300ms 后经 `vault:fs-changed` 事件广播变更 note key 数组。

export const VAULT_FS_CHANGED_EVENT = "vault:fs-changed";

export type UnlistenFn = () => void;

/** 订阅 vault 外部文件变更；非 Tauri 环境（浏览器/mock）返回 no-op。 */
export async function onVaultFsChanged(cb: (keys: string[]) => void): Promise<UnlistenFn> {
  if (!isTauri) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  const unlisten = await listen<string[]>(VAULT_FS_CHANGED_EVENT, (event) => {
    cb(Array.isArray(event.payload) ? event.payload : []);
  });
  return unlisten;
}
