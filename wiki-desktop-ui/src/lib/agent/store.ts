/**
 * thread ↔ vault 关联持久化
 * localStorage key `wiki.agent.threads.v1`，按 vault 根路径 hash 分桶
 */

const STORAGE_KEY = "wiki.agent.threads.v1";

export type AgentThreadRecord = {
  threadId: string;
  title: string;
  createdAt: number;
  model?: string | null;
  /** 审批/沙箱预设 key（只读/自动/全权，命名对齐后续 ModeBar） */
  mode?: string;
  /** 推理强度（null = 跟随模型默认） */
  effort?: string | null;
  pinnedNoteKey?: string | null;
  archived?: boolean;
};

// 简单 djb2 hash，输出 8 位 hex
export function vaultHash(vaultRoot: string): string {
  let hash = 5381;
  for (let i = 0; i < vaultRoot.length; i++) {
    hash = ((hash << 5) + hash + vaultRoot.charCodeAt(i)) | 0;
  }
  // 无符号化并转 8 位 hex
  return (hash >>> 0).toString(16).padStart(8, "0");
}

type StoreShape = Record<string, AgentThreadRecord[]>;

function loadRaw(): StoreShape {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
    return parsed as StoreShape;
  } catch {
    return {};
  }
}

function saveRaw(shape: StoreShape): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(shape));
  } catch {
    // 忽略（如隐私模式）
  }
}

export function listThreads(vaultRoot: string): AgentThreadRecord[] {
  const key = vaultHash(vaultRoot);
  const shape = loadRaw();
  const arr = shape[key];
  if (!Array.isArray(arr)) return [];
  // 过滤非法项
  return arr.filter((it) => typeof it?.threadId === "string" && it.threadId.length > 0);
}

export function addThread(
  vaultRoot: string,
  record: AgentThreadRecord,
): void {
  const key = vaultHash(vaultRoot);
  const shape = loadRaw();
  const bucket = Array.isArray(shape[key]) ? (shape[key] as AgentThreadRecord[]) : [];
  // 去重：同 threadId 更新而非追加
  const idx = bucket.findIndex((it) => it.threadId === record.threadId);
  if (idx === -1) {
    bucket.unshift(record);
  } else {
    bucket[idx] = { ...bucket[idx], ...record };
  }
  shape[key] = bucket;
  saveRaw(shape);
}

export function updateThreadTitle(
  vaultRoot: string,
  threadId: string,
  title: string,
): void {
  const key = vaultHash(vaultRoot);
  const shape = loadRaw();
  const bucket = Array.isArray(shape[key]) ? (shape[key] as AgentThreadRecord[]) : [];
  const idx = bucket.findIndex((it) => it.threadId === threadId);
  if (idx === -1) return;
  bucket[idx] = { ...bucket[idx], title };
  shape[key] = bucket;
  saveRaw(shape);
}

export function archiveThread(vaultRoot: string, threadId: string): void {
  const key = vaultHash(vaultRoot);
  const shape = loadRaw();
  const bucket = Array.isArray(shape[key]) ? (shape[key] as AgentThreadRecord[]) : [];
  const idx = bucket.findIndex((it) => it.threadId === threadId);
  if (idx === -1) return;
  bucket[idx] = { ...bucket[idx], archived: true };
  shape[key] = bucket;
  saveRaw(shape);
}

export function unarchiveThread(vaultRoot: string, threadId: string): void {
  const key = vaultHash(vaultRoot);
  const shape = loadRaw();
  const bucket = Array.isArray(shape[key]) ? (shape[key] as AgentThreadRecord[]) : [];
  const idx = bucket.findIndex((it) => it.threadId === threadId);
  if (idx === -1) return;
  bucket[idx] = { ...bucket[idx], archived: false };
  shape[key] = bucket;
  saveRaw(shape);
}

export function removeThread(vaultRoot: string, threadId: string): void {
  const key = vaultHash(vaultRoot);
  const shape = loadRaw();
  const bucket = Array.isArray(shape[key]) ? (shape[key] as AgentThreadRecord[]) : [];
  shape[key] = bucket.filter((it) => it.threadId !== threadId);
  saveRaw(shape);
}

export function updateThreadModel(
  vaultRoot: string,
  threadId: string,
  model: string | null,
): void {
  const key = vaultHash(vaultRoot);
  const shape = loadRaw();
  const bucket = Array.isArray(shape[key]) ? (shape[key] as AgentThreadRecord[]) : [];
  const idx = bucket.findIndex((it) => it.threadId === threadId);
  if (idx === -1) return;
  bucket[idx] = { ...bucket[idx], model };
  shape[key] = bucket;
  saveRaw(shape);
}

export function getThreadModel(vaultRoot: string, threadId: string): string | null {
  const rec = listThreads(vaultRoot).find((it) => it.threadId === threadId);
  return rec?.model ?? null;
}

export function updateThreadMode(vaultRoot: string, threadId: string, mode: string): void {
  const key = vaultHash(vaultRoot);
  const shape = loadRaw();
  const bucket = Array.isArray(shape[key]) ? (shape[key] as AgentThreadRecord[]) : [];
  const idx = bucket.findIndex((it) => it.threadId === threadId);
  if (idx === -1) return;
  bucket[idx] = { ...bucket[idx], mode };
  shape[key] = bucket;
  saveRaw(shape);
}

export function updateThreadEffort(
  vaultRoot: string,
  threadId: string,
  effort: string | null,
): void {
  const key = vaultHash(vaultRoot);
  const shape = loadRaw();
  const bucket = Array.isArray(shape[key]) ? (shape[key] as AgentThreadRecord[]) : [];
  const idx = bucket.findIndex((it) => it.threadId === threadId);
  if (idx === -1) return;
  bucket[idx] = { ...bucket[idx], effort };
  shape[key] = bucket;
  saveRaw(shape);
}

// 测试辅助：清空
export function __clearStoreForTest(): void {
  try {
    localStorage.removeItem(STORAGE_KEY);
  } catch {
    // ignore
  }
}
