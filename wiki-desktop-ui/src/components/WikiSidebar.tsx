import { useCallback, useEffect, useMemo, useState } from "react";
import { Search, FolderPlus, Plus } from "lucide-react";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import { deleteFolder, listNotes, renameFolder, searchNotes } from "@/api/tauri";
import type { NoteSummary, SearchHitDto, VaultInfo } from "@/api/types";
import { NoteFormDialog } from "@/components/NoteFormDialog";
import { FolderTree } from "@/components/FolderTree";
import { folderPathsOf } from "@/lib/folder-tree";
import { normalizeNoteKey, validateNoteKey } from "@/lib/note-key";

const EXPANDED_KEY = "wiki.folders.expanded.v1";

function loadExpanded(): Set<string> {
  try {
    const raw = localStorage.getItem(EXPANDED_KEY);
    if (!raw) return new Set();
    const arr = JSON.parse(raw) as string[];
    if (!Array.isArray(arr)) return new Set();
    return new Set(arr.filter((s) => typeof s === "string"));
  } catch {
    return new Set();
  }
}

function saveExpanded(s: Set<string>) {
  try {
    localStorage.setItem(EXPANDED_KEY, JSON.stringify([...s]));
  } catch {
    // 忽略持久化失败
  }
}

type Props = {
  selectedKey: string | null;
  onSelect: (key: string) => void;
  refreshToken: number;
  vaultInfo: VaultInfo | null;
  onCreateNote: (key: string, title: string) => void | Promise<void>;
  onFolderChanged?: () => void;
  onHistoryReplacePrefix?: (oldPrefix: string, newPrefix: string) => void;
  onHistoryRemovePrefix?: (prefix: string) => void;
};

export function WikiSidebar({
  selectedKey,
  onSelect,
  refreshToken,
  vaultInfo,
  onCreateNote,
  onFolderChanged,
  onHistoryReplacePrefix,
  onHistoryRemovePrefix,
}: Props) {
  const [query, setQuery] = useState("");
  const [notes, setNotes] = useState<NoteSummary[]>([]);
  const [hits, setHits] = useState<SearchHitDto[] | null>(null);
  const [newOpen, setNewOpen] = useState(false);
  const [createInFolder, setCreateInFolder] = useState<string | null>(null);
  const [renameTarget, setRenameTarget] = useState<string | null>(null);
  const [showNewFolderHint, setShowNewFolderHint] = useState(false);
  const [expanded, setExpanded] = useState<Set<string>>(() => loadExpanded());
  const [activeTag, setActiveTag] = useState<string | null>(null);
  const trimmed = query.trim();

  // 持久化展开状态
  useEffect(() => {
    saveExpanded(expanded);
  }, [expanded]);

  const toggle = useCallback((path: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }, []);

  // 标签聚合：按 count 降序取前 20
  const tagChips = useMemo(() => {
    const m = new Map<string, number>();
    for (const n of notes) {
      for (const t of n.tags) {
        m.set(t, (m.get(t) ?? 0) + 1);
      }
    }
    return [...m.entries()]
      .sort((a, b) => b[1] - a[1])
      .slice(0, 20)
      .map(([tag, count]) => ({ tag, count }));
  }, [notes]);

  // 激活标签时的过滤
  const filteredNotes = useMemo(() => {
    if (!activeTag) return notes;
    return notes.filter((n) => n.tags.includes(activeTag));
  }, [notes, activeTag]);

  const filteredHits = useMemo(() => {
    if (!hits) return [];
    if (!activeTag) return hits;
    // hits 只有 key，用 notes 建 key→tags 映射过滤
    const tagMap = new Map(notes.map((n) => [n.key, n.tags] as const));
    return hits.filter((h) => (tagMap.get(h.note_key) ?? []).includes(activeTag));
  }, [hits, notes, activeTag]);

  // 加载列表
  useEffect(() => {
    let cancelled = false;
    listNotes()
      .then((data) => {
        if (!cancelled) setNotes(data);
      })
      .catch(() => {
        if (!cancelled) setNotes([]);
      });
    return () => {
      cancelled = true;
    };
  }, [refreshToken]);

  // 搜索（输入非空时展示 search_notes 结果）
  useEffect(() => {
    if (!trimmed) {
      setHits(null);
      return;
    }
    let cancelled = false;
    const t = window.setTimeout(() => {
      searchNotes(trimmed, 20)
        .then((data) => {
          if (!cancelled) setHits(data);
        })
        .catch(() => {
          if (!cancelled) setHits([]);
        });
    }, 180);
    return () => {
      window.clearTimeout(t);
      cancelled = true;
    };
  }, [trimmed]);

  const isSearching = trimmed.length > 0;

  const handleCreate = async (raw: string) => {
    const fullKey = createInFolder ? `${createInFolder}/${raw}` : raw;
    const normalized = normalizeNoteKey(fullKey);
    const err = validateNoteKey(fullKey);
    if (err) throw new Error(err);
    if (notes.some((n) => n.key === normalized)) {
      throw new Error("已存在同名笔记");
    }
    await onCreateNote(normalized, normalized);
    // 自动展开祖先
    const ancestors = folderPathsOf(normalized);
    if (ancestors.length > 0) {
      setExpanded((prev) => {
        const next = new Set(prev);
        for (const a of ancestors) next.add(a);
        return next;
      });
    }
    setNewOpen(false);
    setCreateInFolder(null);
  };

  const createValidate = (raw: string): string | null => {
    const fullKey = createInFolder ? `${createInFolder}/${raw}` : raw;
    const err = validateNoteKey(fullKey);
    if (err) return err;
    const normalized = normalizeNoteKey(fullKey);
    if (notes.some((n) => n.key === normalized)) return "已存在同名笔记";
    return null;
  };

  const handleCreateInFolder = useCallback((folderPath: string) => {
    setCreateInFolder(folderPath);
    setNewOpen(true);
  }, []);

  const handleRenameFolder = useCallback(
    async (folderPath: string, rawNewPrefix: string) => {
      const newPrefix = normalizeNoteKey(rawNewPrefix);
      const err = validateNoteKey(rawNewPrefix);
      if (err) throw new Error(err);
      if (newPrefix === folderPath) return;
      // 统计将移动的笔记数
      const affected = notes.filter((n) => n.key === folderPath || n.key.startsWith(folderPath + "/"));
      const n = affected.length;
      const ok = window.confirm(`将移动 ${n} 篇笔记：${folderPath} → ${newPrefix}，是否继续？`);
      if (!ok) throw new Error("已取消");
      const res = await renameFolder(folderPath, newPrefix, true);
      if (res.failed.length > 0) {
        window.alert(`部分失败：\n${res.failed.map((f) => `${f.key}: ${f.error}`).join("\n")}`);
      }
      // 展开新路径祖先
      // 展开新前缀的所有祖先 + 自身
      const toExpand = folderPathsOf(newPrefix);
      // 若 newPrefix 本身含 "/"，需展开其自身
      if (newPrefix.includes("/")) {
        // folderPathsOf(newPrefix) 对 "a/b" 返回 ["a"]，需补上 "a/b"
        // 通用：逐段构造
        const segs = newPrefix.split("/");
        let cur = "";
        const all: string[] = [];
        for (const seg of segs) {
          cur = cur ? `${cur}/${seg}` : seg;
          all.push(cur);
        }
        // 去掉最后一段之前已在 toExpand 的，取并集
        const merged = new Set([...toExpand, ...all]);
        setExpanded((prev) => {
          const next = new Set<string>();
          for (const p of prev) {
            if (p === folderPath || p.startsWith(folderPath + "/")) {
              const suffix = p.slice(folderPath.length);
              next.add(newPrefix + suffix);
            } else {
              next.add(p);
            }
          }
          for (const a of merged) next.add(a);
          return next;
        });
      } else {
        setExpanded((prev) => {
          const next = new Set<string>();
          for (const p of prev) {
            if (p === folderPath || p.startsWith(folderPath + "/")) {
              const suffix = p.slice(folderPath.length);
              next.add(newPrefix + suffix);
            } else {
              next.add(p);
            }
          }
          // 单段文件夹也要展开自身
          next.add(newPrefix);
          return next;
        });
      }
      onHistoryReplacePrefix?.(folderPath, newPrefix);
      onFolderChanged?.();
    },
    [notes, onFolderChanged, onHistoryReplacePrefix],
  );

  const handleDeleteFolder = useCallback(
    async (folderPath: string, noteCount: number) => {
      const ok = window.confirm(`将删除文件夹「${folderPath}」下的 ${noteCount} 篇笔记，不可恢复，是否继续？`);
      if (!ok) return;
      const res = await deleteFolder(folderPath);
      if (res.failed.length > 0) {
        window.alert(`部分失败：\n${res.failed.map((f) => `${f.key}: ${f.error}`).join("\n")}`);
      }
      // 清理展开状态中被删路径
      setExpanded((prev) => {
        const next = new Set<string>();
        for (const p of prev) {
          if (p === folderPath || p.startsWith(folderPath + "/")) continue;
          next.add(p);
        }
        return next;
      });
      onHistoryRemovePrefix?.(folderPath);
      onFolderChanged?.();
    },
    [onFolderChanged, onHistoryRemovePrefix],
  );

  const handleRenameSubmit = async (raw: string) => {
    if (!renameTarget) return;
    await handleRenameFolder(renameTarget, raw);
    setRenameTarget(null);
  };

  const renameValidate = (raw: string): string | null => validateNoteKey(raw);

  const newDialogTitle = createInFolder ? `在 ${createInFolder} 新建笔记` : "新建笔记";
  const newDialogHint = createInFolder
    ? `将创建于文件夹 ${createInFolder} 下`
    : "将作为文件名保存，可用 / 分层；文件夹随首篇笔记创建";
  const newDialogPlaceholder = createInFolder ? "输入笔记名" : "例如 my-note 或 folder/note";

  return (
    <div className="flex h-full flex-col">
      {/* 顶部：vault 信息 */}
      <div className="border-b px-4 py-3">
        <p className="text-xs text-muted-foreground">仓库</p>
        <p className="truncate text-sm font-medium" title={vaultInfo?.root ?? ""}>
          {vaultInfo ? vaultInfo.root : "加载中…"}
        </p>
        <p className="text-xs text-muted-foreground">{vaultInfo ? `${vaultInfo.note_count} 篇笔记` : ""}</p>
      </div>

      {/* 搜索框 + 新建按钮 */}
      <div className="p-3">
        <div className="flex items-center gap-2">
          <div className="relative flex-1">
            <Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              placeholder="搜索笔记…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              className="pl-9"
            />
          </div>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="size-9 shrink-0"
            onClick={() => {
              setCreateInFolder(null);
              setNewOpen(true);
            }}
            title="新建笔记"
            aria-label="新建笔记"
          >
            <Plus className="size-4" />
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="size-9 shrink-0"
            onClick={() => setShowNewFolderHint(true)}
            title="新建文件夹"
            aria-label="新建文件夹"
          >
            <FolderPlus className="size-4" />
          </Button>
        </div>
        {isSearching && <p className="mt-2 text-xs text-muted-foreground">搜索：{trimmed}</p>}
        {/* 标签过滤 chip 行（横向滚动，单选） */}
        {tagChips.length > 0 && (
          <div className="mt-2 flex gap-1.5 overflow-x-auto pb-1 scrollbar-thin">
            {tagChips.map(({ tag, count }) => {
              const active = activeTag === tag;
              return (
                <button
                  key={tag}
                  type="button"
                  onClick={() => setActiveTag((prev) => (prev === tag ? null : tag))}
                  className="shrink-0"
                  aria-pressed={active}
                  aria-label={`按标签 ${tag} 过滤`}
                >
                  <Badge variant={active ? "default" : "secondary"} className="gap-1 px-2 py-0.5 text-xs">
                    {tag}
                    <span className="text-[10px] opacity-70">{count}</span>
                  </Badge>
                </button>
              );
            })}
          </div>
        )}
      </div>

      {/* 列表：搜索态保持平铺，非搜索态用文件夹树 */}
      <div className="flex-1 overflow-y-auto px-2 pb-3">
        {isSearching ? (
          hits === null ? (
            <p className="px-2 py-6 text-center text-sm text-muted-foreground">搜索中…</p>
          ) : filteredHits.length === 0 ? (
            <p className="px-2 py-6 text-center text-sm text-muted-foreground">没有匹配的笔记</p>
          ) : (
            <ul className="space-y-1">
              {filteredHits.map((h) => (
                <li key={h.note_key}>
                  <button
                    type="button"
                    onClick={() => onSelect(h.note_key)}
                    className={cn(
                      "flex w-full flex-col gap-1 rounded-md border px-3 py-2.5 text-left transition-colors",
                      selectedKey === h.note_key
                        ? "border-primary/40 bg-accent"
                        : "border-transparent hover:bg-accent/60",
                    )}
                  >
                    <span className="line-clamp-1 text-sm font-medium">{h.title}</span>
                    <span className="line-clamp-2 text-xs text-muted-foreground">{h.snippet}</span>
                    <span className="text-xs text-muted-foreground">匹配分 {h.score.toFixed(1)}</span>
                  </button>
                </li>
              ))}
            </ul>
          )
        ) : (
          <FolderTree
            notes={filteredNotes}
            selectedKey={selectedKey}
            expanded={expanded}
            onToggle={toggle}
            onSelectNote={onSelect}
            onCreateInFolder={handleCreateInFolder}
            onRenameFolder={(path) => setRenameTarget(path)}
            onDeleteFolder={handleDeleteFolder}
          />
        )}
      </div>

      {newOpen && (
        <NoteFormDialog
          title={newDialogTitle}
          label="标题"
          placeholder={newDialogPlaceholder}
          hint={newDialogHint}
          submitText="创建"
          validate={createValidate}
          onSubmit={handleCreate}
          onClose={() => {
            setNewOpen(false);
            setCreateInFolder(null);
          }}
        />
      )}

      {renameTarget && (
        <NoteFormDialog
          title="重命名文件夹"
          label="新路径"
          initial={renameTarget}
          placeholder="输入新的文件夹路径"
          hint="将移动该文件夹下所有笔记"
          submitText="重命名"
          validate={renameValidate}
          onSubmit={handleRenameSubmit}
          onClose={() => setRenameTarget(null)}
        />
      )}

      {showNewFolderHint && (
        <NoteFormDialog
          title="新建文件夹"
          label="文件夹路径"
          placeholder="例如 folder/sub"
          hint="文件夹随首篇笔记创建；请输入 folder/note 形式的笔记路径来创建首篇笔记"
          submitText="去新建笔记"
          validate={renameValidate}
          onSubmit={async (raw) => {
            const normalized = normalizeNoteKey(raw);
            // 新建文件夹本质是提示用户去新建笔记，这里直接打开新建对话框并预填文件夹
            setShowNewFolderHint(false);
            setCreateInFolder(normalized);
            setNewOpen(true);
          }}
          onClose={() => setShowNewFolderHint(false)}
        />
      )}
    </div>
  );
}
