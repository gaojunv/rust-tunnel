import { useEffect, useMemo, useState } from "react";
import { Search, FileText, Clock3, Plus } from "lucide-react";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { listNotes, searchNotes } from "@/api/tauri";
import type { NoteSummary, SearchHitDto, VaultInfo } from "@/api/types";
import { NoteFormDialog } from "@/components/NoteFormDialog";
import { normalizeNoteKey, validateNoteKey } from "@/lib/note-key";

function formatRelative(sec: number): string {
  const diff = Math.floor(Date.now() / 1000) - sec;
  if (diff < 60) return "刚刚";
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时前`;
  return `${Math.floor(diff / 86400)} 天前`;
}

type Props = {
  selectedKey: string | null;
  onSelect: (key: string) => void;
  refreshToken: number;
  vaultInfo: VaultInfo | null;
  onCreateNote: (key: string, title: string) => void | Promise<void>;
};

export function WikiSidebar({ selectedKey, onSelect, refreshToken, vaultInfo, onCreateNote }: Props) {
  const [query, setQuery] = useState("");
  const [notes, setNotes] = useState<NoteSummary[]>([]);
  const [hits, setHits] = useState<SearchHitDto[] | null>(null);
  const [newOpen, setNewOpen] = useState(false);
  const trimmed = query.trim();

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

  const searchHitKeys = useMemo(() => {
    if (!hits) return null;
    return new Set(hits.map((h) => h.note_key));
  }, [hits]);

  const handleCreate = async (raw: string) => {
    const normalized = normalizeNoteKey(raw);
    const err = validateNoteKey(raw);
    if (err) throw new Error(err);
    if (notes.some((n) => n.key === normalized)) {
      throw new Error("已存在同名笔记");
    }
    await onCreateNote(normalized, normalized);
    setNewOpen(false);
  };

  const createValidate = (raw: string): string | null => {
    const err = validateNoteKey(raw);
    if (err) return err;
    const normalized = normalizeNoteKey(raw);
    if (notes.some((n) => n.key === normalized)) return "已存在同名笔记";
    return null;
  };

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
            onClick={() => setNewOpen(true)}
            title="新建笔记"
            aria-label="新建笔记"
          >
            <Plus className="size-4" />
          </Button>
        </div>
        {isSearching && <p className="mt-2 text-xs text-muted-foreground">搜索：{trimmed}</p>}
      </div>

      {/* 列表 */}
      <div className="flex-1 overflow-y-auto px-2 pb-3">
        {isSearching ? (
          hits === null ? (
            <p className="px-2 py-6 text-center text-sm text-muted-foreground">搜索中…</p>
          ) : hits.length === 0 ? (
            <p className="px-2 py-6 text-center text-sm text-muted-foreground">没有匹配的笔记</p>
          ) : (
            <ul className="space-y-1">
              {hits.map((h) => (
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
        ) : notes.length === 0 ? (
          <div className="flex flex-col items-center gap-2 px-2 py-10 text-center">
            <FileText className="size-8 text-muted-foreground/60" />
            <p className="text-sm font-medium">还没有笔记</p>
            <p className="text-xs text-muted-foreground">在编辑器中新建一篇，或检查仓库路径是否正确。</p>
          </div>
        ) : (
          <ul className="space-y-1">
            {notes.map((n) => {
              const dimmed = searchHitKeys ? !searchHitKeys.has(n.key) : false;
              return (
                <li key={n.key}>
                  <button
                    type="button"
                    onClick={() => onSelect(n.key)}
                    className={cn(
                      "flex w-full flex-col gap-1.5 rounded-md border px-3 py-2.5 text-left transition-colors",
                      selectedKey === n.key ? "border-primary/40 bg-accent" : "border-transparent hover:bg-accent/60",
                      dimmed && "opacity-60",
                    )}
                  >
                    <span className="line-clamp-1 text-sm font-medium">{n.title}</span>
                    <span className="flex flex-wrap gap-1">
                      {n.tags.length > 0 ? (
                        n.tags.map((t) => (
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
                      {formatRelative(n.modified)}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        )}
      </div>

      {newOpen && (
        <NoteFormDialog
          title="新建笔记"
          label="标题"
          placeholder="例如 my-note 或 folder/note"
          hint="将作为文件名保存，可用 / 分层"
          submitText="创建"
          validate={createValidate}
          onSubmit={handleCreate}
          onClose={() => setNewOpen(false)}
        />
      )}
    </div>
  );
}
