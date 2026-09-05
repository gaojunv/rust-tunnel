// 与 Rust 侧 DTO 对齐（serde 默认 snake_case，字段名保持一致）

/** 列表用摘要 */
export interface NoteSummary {
  key: string;
  title: string;
  tags: string[];
  /** unix 秒 */
  modified: number;
  ref_id?: string | null;
}

/** 单篇笔记详情 */
export interface NoteDto {
  key: string;
  title: string;
  aliases: string[];
  tags: string[];
  body: string;
  /** unix 秒 */
  modified: number;
  ref_id?: string | null;
}

/** 搜索命中 */
export interface SearchHitDto {
  note_key: string;
  title: string;
  snippet: string;
  score: number;
}

/** 图谱节点 */
export interface GraphNode {
  key: string;
  title: string;
}

/** 图谱边 */
export interface GraphEdge {
  from: string;
  to: string;
}

/** 整图 */
export interface GraphDto {
  nodes: GraphNode[];
  edges: GraphEdge[];
}

/** 仓库信息 */
export interface VaultInfo {
  root: string;
  note_count: number;
}
/** 文件夹批量移动结果 */
export interface MovedEntry {
  from_key: string;
  to_key: string;
}
export interface FailedEntry {
  key: string;
  error: string;
}
export interface RenameFolderResult {
  moved: MovedEntry[];
  failed: FailedEntry[];
  link_rewritten: string[];
  rewritten_count: number;
}
export interface DeleteFolderResult {
  deleted: string[];
  failed: FailedEntry[];
}
