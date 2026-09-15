// 文件夹树组件：递归渲染，交互全部经 props 回调
import { memo, useMemo, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { buildTree, type TreeNode } from "@/lib/folder-tree";
import type { NoteSummary } from "@/api/types";
import { ContextMenu, ContextMenuItem } from "@/components/ContextMenu";
import {
  BookOpen,
  ChevronDown,
  ChevronRight,
  Clock3,
  FilePlus,
  FileText,
  Folder,
  FolderOpen,
  FolderPlus,
  Pencil,
  Plus,
  Trash2,
} from "lucide-react";

function formatRelative(sec: number): string {
  const diff = Math.floor(Date.now() / 1000) - sec;
  if (diff < 60) return "刚刚";
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时前`;
  return `${Math.floor(diff / 86400)} 天前`;
}

type Props = {
  notes: NoteSummary[];
  selectedKey: string | null;
  expanded: Set<string>;
  onToggle: (path: string) => void;
  onSelectNote: (key: string) => void;
  onCreateInFolder: (folderPath: string) => void;
  onCreateSubfolder: (folderPath: string) => void;
  onRenameFolder: (folderPath: string) => void;
  onDeleteFolder: (folderPath: string, noteCount: number) => void;
  onRenameNote: (key: string) => void;
  onDeleteNote: (key: string) => void;
};

/** 右键菜单锚点：viewport 坐标 */
type MenuAnchor = { x: number; y: number };

const FolderRow = memo(function FolderRow({
  node,
  depth,
  isExpanded,
  onToggle,
  onCreateInFolder,
  onCreateSubfolder,
  onRenameFolder,
  onDeleteFolder,
  children,
}: {
  node: Extract<TreeNode, { kind: "folder" }>;
  depth: number;
  isExpanded: boolean;
  onToggle: (path: string) => void;
  onCreateInFolder: (folderPath: string) => void;
  onCreateSubfolder: (folderPath: string) => void;
  onRenameFolder: (folderPath: string) => void;
  onDeleteFolder: (folderPath: string, noteCount: number) => void;
  children: React.ReactNode;
}) {
  const [menu, setMenu] = useState<MenuAnchor | null>(null);
  const closeMenu = () => setMenu(null);
  return (
    <li>
      <div
        className={cn(
          "group flex items-center gap-1 rounded-md px-1 py-1 hover:bg-accent/60",
        )}
        style={{ paddingLeft: depth * 12 + 4 }}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
      >
        <button
          type="button"
          onClick={() => onToggle(node.path)}
          className="flex flex-1 items-center gap-1.5 text-left"
          aria-label={isExpanded ? "折叠" : "展开"}
        >
          {isExpanded ? (
            <ChevronDown className="size-3.5 shrink-0 text-muted-foreground" />
          ) : (
            <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" />
          )}
          {isExpanded ? (
            <FolderOpen className="size-3.5 shrink-0 text-muted-foreground" />
          ) : (
            <Folder className="size-3.5 shrink-0 text-muted-foreground" />
          )}
          <span className="truncate text-sm font-medium">{node.name}</span>
          <span className="text-xs text-muted-foreground">{node.noteCount}</span>
        </button>
        <span className="flex items-center gap-0.5 opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100">
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="size-6"
            onClick={() => onCreateInFolder(node.path)}
            title="在此新建笔记"
            aria-label="在此新建笔记"
          >
            <Plus className="size-3" />
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="size-6"
            onClick={() => onRenameFolder(node.path)}
            title="重命名文件夹"
            aria-label="重命名文件夹"
          >
            <Pencil className="size-3" />
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="size-6 hover:bg-destructive/10 hover:text-destructive"
            onClick={() => onDeleteFolder(node.path, node.noteCount)}
            title="删除文件夹"
            aria-label="删除文件夹"
          >
            <Trash2 className="size-3" />
          </Button>
        </span>
      </div>
      {menu && (
        <ContextMenu x={menu.x} y={menu.y} onClose={closeMenu}>
          <ContextMenuItem
            label="新建笔记"
            icon={<FilePlus className="size-3.5 shrink-0 text-muted-foreground" />}
            onSelect={() => {
              closeMenu();
              onCreateInFolder(node.path);
            }}
          />
          <ContextMenuItem
            label="新建子文件夹"
            icon={<FolderPlus className="size-3.5 shrink-0 text-muted-foreground" />}
            onSelect={() => {
              closeMenu();
              onCreateSubfolder(node.path);
            }}
          />
          <ContextMenuItem
            label="重命名"
            icon={<Pencil className="size-3.5 shrink-0 text-muted-foreground" />}
            onSelect={() => {
              closeMenu();
              onRenameFolder(node.path);
            }}
          />
          <ContextMenuItem
            label="删除"
            danger
            icon={<Trash2 className="size-3.5 shrink-0" />}
            onSelect={() => {
              closeMenu();
              onDeleteFolder(node.path, node.noteCount);
            }}
          />
        </ContextMenu>
      )}
      {isExpanded && <ul className="space-y-0.5">{children}</ul>}
    </li>
  );
});

const NoteRow = memo(function NoteRow({
  node,
  depth,
  selectedKey,
  onSelectNote,
  onRenameNote,
  onDeleteNote,
}: {
  node: Extract<TreeNode, { kind: "note" }>;
  depth: number;
  selectedKey: string | null;
  onSelectNote: (key: string) => void;
  onRenameNote: (key: string) => void;
  onDeleteNote: (key: string) => void;
}) {
  const [menu, setMenu] = useState<MenuAnchor | null>(null);
  const closeMenu = () => setMenu(null);
  const isSelected = selectedKey === node.note.key;
  return (
    <li style={{ paddingLeft: depth * 12 + 4 }} className="pr-1">
      <button
        type="button"
        onClick={() => onSelectNote(node.note.key)}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
        className={cn(
          "flex w-full flex-col gap-1 rounded-md border px-3 py-2 text-left transition-colors",
          isSelected ? "border-primary/40 bg-accent" : "border-transparent hover:bg-accent/60",
        )}
      >
        <span className="flex items-center gap-1.5">
          <FileText className="size-3 shrink-0 text-muted-foreground" />
          <span className="line-clamp-1 text-sm font-medium">{node.note.title}</span>
        </span>
        <span className="flex flex-wrap gap-1">
          {node.note.tags.length > 0 ? (
            node.note.tags.map((t) => (
              <Badge key={t} variant="secondary" className="px-1.5 py-0 text-[10px]">
                {t}
              </Badge>
            ))
          ) : (
            <span className="text-xs text-muted-foreground">无标签</span>
          )}
        </span>
        <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
          <Clock3 className="size-3" />
          {formatRelative(node.note.modified)}
        </span>
      </button>
      {menu && (
        <ContextMenu x={menu.x} y={menu.y} onClose={closeMenu}>
          <ContextMenuItem
            label="打开"
            icon={<BookOpen className="size-3.5 shrink-0 text-muted-foreground" />}
            onSelect={() => {
              closeMenu();
              onSelectNote(node.note.key);
            }}
          />
          <ContextMenuItem
            label="重命名"
            icon={<Pencil className="size-3.5 shrink-0 text-muted-foreground" />}
            onSelect={() => {
              closeMenu();
              onRenameNote(node.note.key);
            }}
          />
          <ContextMenuItem
            label="删除"
            danger
            icon={<Trash2 className="size-3.5 shrink-0" />}
            onSelect={() => {
              closeMenu();
              onDeleteNote(node.note.key);
            }}
          />
        </ContextMenu>
      )}
    </li>
  );
});

export const FolderTree = memo(function FolderTree({
  notes,
  selectedKey,
  expanded,
  onToggle,
  onSelectNote,
  onCreateInFolder,
  onCreateSubfolder,
  onRenameFolder,
  onDeleteFolder,
  onRenameNote,
  onDeleteNote,
}: Props) {
  // notes 引用不变时复用整棵树：WikiSidebar 的 listNotes setState 在内容一致时仍换引用，
  // 此处 useMemo 至少保证父级其他 state 变化（query 输入等）不触发重建
  const tree = useMemo(() => buildTree(notes), [notes]);

  if (tree.length === 0) {
    return (
      <div className="flex flex-col items-center gap-2 px-2 py-10 text-center">
        <FileText className="size-8 text-muted-foreground/60" />
        <p className="text-sm font-medium">还没有笔记</p>
        <p className="text-xs text-muted-foreground">在编辑器中新建一篇，或检查仓库路径是否正确。</p>
      </div>
    );
  }

  const renderNodes = (nodes: TreeNode[], depth: number): React.ReactNode[] => {
    return nodes.map((n) => {
      if (n.kind === "folder") {
        const isExpanded = expanded.has(n.path);
        return (
          <FolderRow
            key={n.path}
            node={n}
            depth={depth}
            isExpanded={isExpanded}
            onToggle={onToggle}
            onCreateInFolder={onCreateInFolder}
            onCreateSubfolder={onCreateSubfolder}
            onRenameFolder={onRenameFolder}
            onDeleteFolder={onDeleteFolder}
          >
            {renderNodes(n.children, depth + 1)}
          </FolderRow>
        );
      }
      return (
        <NoteRow
          key={n.note.key}
          node={n}
          depth={depth}
          selectedKey={selectedKey}
          onSelectNote={onSelectNote}
          onRenameNote={onRenameNote}
          onDeleteNote={onDeleteNote}
        />
      );
    });
  };

  return <ul className="space-y-0.5 px-1">{renderNodes(tree, 0)}</ul>;
});
