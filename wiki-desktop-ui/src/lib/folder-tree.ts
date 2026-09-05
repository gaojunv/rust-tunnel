/**
 * 文件夹树纯逻辑：按 key 的 "/" 切段嵌套
 * 文件夹在前、各自按 name.localeCompare(name2, "zh") 排序；noteCount 递归统计
 */
import type { NoteSummary } from "@/api/types";

export type NoteNode = { kind: "note"; name: string; note: NoteSummary };
export type FolderNode = { kind: "folder"; name: string; path: string; children: TreeNode[]; noteCount: number };
export type TreeNode = FolderNode | NoteNode;

/**
 * 排序比较器：中文友好，大小写不敏感由 localeCompare 处理
 */
function compareName(a: string, b: string): number {
  return a.localeCompare(b, "zh");
}

/**
 * 按 "/" 切段构树
 */
export function buildTree(notes: NoteSummary[]): TreeNode[] {
  // 内部中间节点：path -> { name, children: Map<name, ...>, notes: NoteNode[] }
  type Internal = { name: string; path: string; subfolders: Map<string, Internal>; notes: NoteNode[] };
  const root: Internal = { name: "", path: "", subfolders: new Map(), notes: [] };

  for (const note of notes) {
    const segs = note.key.split("/");
    // 最后一段是文件名，前面的都是文件夹
    const folderSegs = segs.slice(0, -1);
    const leafName = segs[segs.length - 1] ?? note.key;
    let cur = root;
    let curPath = "";
    for (const seg of folderSegs) {
      curPath = curPath ? `${curPath}/${seg}` : seg;
      let child = cur.subfolders.get(seg);
      if (!child) {
        child = { name: seg, path: curPath, subfolders: new Map(), notes: [] };
        cur.subfolders.set(seg, child);
      }
      cur = child;
    }
    cur.notes.push({ kind: "note", name: leafName, note });
  }

  function toTreeNode(internal: Internal): FolderNode {
    // 先收集子文件夹和笔记节点
    const folderNodes: FolderNode[] = [];
    for (const sub of internal.subfolders.values()) {
      folderNodes.push(toTreeNode(sub));
    }
    const noteNodes: NoteNode[] = [...internal.notes];

    // 各自按 name.localeCompare 排序
    folderNodes.sort((a, b) => compareName(a.name, b.name));
    noteNodes.sort((a, b) => compareName(a.name, b.name));

    // 合并：文件夹在前
    const children: TreeNode[] = [...folderNodes, ...noteNodes];
    // 递归统计笔记数：所有后代笔记 + 直属笔记
    const noteCount = countNotes(children);

    return {
      kind: "folder",
      name: internal.name,
      path: internal.path,
      children,
      noteCount,
    };
  }

  function countNotes(nodes: TreeNode[]): number {
    let c = 0;
    for (const n of nodes) {
      if (n.kind === "note") c += 1;
      else c += n.noteCount;
    }
    return c;
  }

  // 根为虚拟容器，不作为 FolderNode 返回；返回其 children（已拍平一层）
  const topFolders: FolderNode[] = [];
  for (const sub of root.subfolders.values()) {
    topFolders.push(toTreeNode(sub));
  }
  const topNotes: NoteNode[] = [...root.notes];

  topFolders.sort((a, b) => compareName(a.name, b.name));
  topNotes.sort((a, b) => compareName(a.name, b.name));

  return [...topFolders, ...topNotes];
}

/**
 * 返回某 key 的所有祖先文件夹路径
 * 例如 "a/b/c" -> ["a", "a/b"]
 */
export function folderPathsOf(key: string): string[] {
  const segs = key.split("/");
  // 最后一段是文件名，祖先仅到倒数第二段
  if (segs.length <= 1) return [];
  const out: string[] = [];
  let cur = "";
  for (let i = 0; i < segs.length - 1; i++) {
    cur = cur ? `${cur}/${segs[i]}` : segs[i];
    out.push(cur);
  }
  return out;
}
