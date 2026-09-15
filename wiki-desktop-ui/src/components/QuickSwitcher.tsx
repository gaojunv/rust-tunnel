import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Plus, Search } from "lucide-react";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { ScrollArea } from "@/components/ui/scroll-area";
import { listNotes } from "@/api/tauri";
import type { NoteSummary } from "@/api/types";
import { fuzzyScorePrepared, matchIndices, prepareHaystack } from "@/lib/fuzzy";
import { resolveQuickCreate } from "@/lib/quick-create";

/** 输入防抖：100ms（任务要求 80-120ms 区间） */
const INPUT_DEBOUNCE_MS = 100;

type Props = {
  open: boolean;
  onClose: () => void;
  onSelect: (key: string) => void;
  onCreate?: (key: string, title: string) => void | Promise<void>;
};

type Scored = {
  note: NoteSummary;
  score: number;
  winField: "title" | "key" | "tags" | null;
};

const TitleWithHighlight = memo(function TitleWithHighlight({ title, query }: { title: string; query: string }) {
  const indices = useMemo(() => {
    if (!query) return null;
    const idx = matchIndices(title, query);
    return idx.length === query.length ? new Set(idx) : null;
  }, [title, query]);

  if (!indices) return <>{title}</>;

  return (
    <>
      {Array.from({ length: title.length }, (_, i) => {
        const ch = title[i];
        if (indices.has(i)) {
          return (
            <mark key={i} className="bg-transparent p-0 text-primary font-semibold">
              {ch}
            </mark>
          );
        }
        return <span key={i}>{ch}</span>;
      })}
    </>
  );
});

export function QuickSwitcher({ open, onClose, onSelect, onCreate }: Props) {
  const [query, setQuery] = useState("");
  const [notes, setNotes] = useState<NoteSummary[] | null>(null);
  const [activeIndex, setActiveIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const itemRefs = useRef<Map<number, HTMLButtonElement>>(new Map());

  // 输入防抖：query 即时跟随输入框（保证输入无延迟），debouncedQuery 延迟 100ms
  // 供 fuzzy 计算与快捷新建判定消费。每键只做一次 setTimeout 调度，而非 N 次全量扫描。
  const [debouncedQuery, setDebouncedQuery] = useState("");
  useEffect(() => {
    const t = window.setTimeout(() => setDebouncedQuery(query), INPUT_DEBOUNCE_MS);
    return () => window.clearTimeout(t);
  }, [query]);
  const trimmed = debouncedQuery.trim();

  // 每次打开：重置状态、聚焦、拉取最新列表
  useEffect(() => {
    if (!open) return;
    setQuery("");
    setDebouncedQuery("");
    setActiveIndex(0);
    setNotes(null);
    // autofocus after portal mounted
    requestAnimationFrame(() => inputRef.current?.focus());
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
  }, [open]);

  // 预计算小写串：notes 拉取后一次性 toLowerCase + tags.join，
  // 每键扫描时零分配（fuzzyScorePrepared 直接用缓存）。
  const prepared = useMemo(() => {
    if (notes === null) return null;
    return notes.map((n) => ({
      note: n,
      title: prepareHaystack(n.title),
      key: prepareHaystack(n.key),
      tags: prepareHaystack(n.tags.join(" ")),
    }));
  }, [notes]);
  const noteKeys = useMemo(() => (notes ?? []).map((n) => n.key), [notes]);

  const scored: Scored[] = useMemo(() => {
    if (prepared === null) return [];
    if (!trimmed) {
      return [...prepared]
        .sort((a, b) => b.note.modified - a.note.modified)
        .slice(0, 15)
        .map((p) => ({ note: p.note, score: 0, winField: null }) as Scored);
    }
    const qLower = trimmed.toLowerCase();
    const out: Scored[] = [];
    for (const p of prepared) {
      const sTitle = fuzzyScorePrepared(p.title, qLower);
      const sKey = fuzzyScorePrepared(p.key, qLower);
      const sTags = fuzzyScorePrepared(p.tags, qLower);
      let best: number | null = null;
      let win: Scored["winField"] = null;
      if (sTitle !== null && (best === null || sTitle > best)) {
        best = sTitle;
        win = "title";
      }
      if (sKey !== null && (best === null || sKey > best)) {
        best = sKey;
        win = "key";
      }
      if (sTags !== null && (best === null || sTags > best)) {
        best = sTags;
        win = "tags";
      }
      if (best !== null) out.push({ note: p.note, score: best, winField: win });
    }
    out.sort((a, b) => {
      if (b.score !== a.score) return b.score - a.score;
      return b.note.modified - a.note.modified;
    });
    return out.slice(0, 15);
  }, [prepared, trimmed]);

  // 快捷新建判定：消费防抖后的 trimmed；existingKeys 用 memo 化的 noteKeys。
  // 空结果快捷新建逻辑保持不变（createState.kind ready/invalid 时展示创建项）。
  const createState = useMemo(
    () =>
      resolveQuickCreate(
        trimmed,
        noteKeys,
        onCreate !== undefined && notes !== null,
      ),
    [trimmed, noteKeys, onCreate, notes],
  );
  // 创建项可交互（ready 可执行；invalid 展示错误提示，不可执行）
  const showCreate = createState.kind === "ready" || createState.kind === "invalid";
  // 列表总行数 = 匹配结果 + 创建项（若展示）
  const rowCount = scored.length + (showCreate ? 1 : 0);

  // 结果变化时把 activeIndex 约束回合法区间
  useEffect(() => {
    if (rowCount === 0) {
      setActiveIndex(0);
      return;
    }
    if (activeIndex >= rowCount) setActiveIndex(0);
  }, [rowCount, activeIndex]);

  // 保持活动项可见
  useEffect(() => {
    const el = itemRefs.current.get(activeIndex);
    el?.scrollIntoView({ block: "nearest" });
  }, [activeIndex]);

  const handleSelect = useCallback(
    (key: string) => {
      onSelect(key);
      onClose();
    },
    [onSelect, onClose],
  );

  const handleCreate = useCallback(
    (key: string) => {
      // App 的 handleCreateNote 自带 alert 兜底；此处双保险：仅 ready 态调用
      void onCreate?.(key, key);
      onClose();
    },
    [onCreate, onClose],
  );

  const handleInputKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLInputElement>) => {
      if (rowCount === 0 && e.key !== "Escape") return;
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setActiveIndex((i) => (i + 1) % rowCount);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setActiveIndex((i) => (i - 1 + rowCount) % rowCount);
      } else if (e.key === "Enter") {
        e.preventDefault();
        if (activeIndex < scored.length) {
          const cur = scored[activeIndex];
          if (cur) handleSelect(cur.note.key);
        } else if (showCreate && createState.kind === "ready") {
          handleCreate(createState.key);
        }
        // invalid 创建项聚焦时 Enter 无操作（提示已展示）
      } else if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    },
    [rowCount, scored, activeIndex, handleSelect, showCreate, createState, handleCreate, onClose],
  );

  if (!open) return null;

  const overlay = (
    <div
      data-modal-open=""
      className="fixed inset-0 z-50 flex items-start justify-center bg-black/60 pt-[15vh]"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="w-[min(92vw,560px)] overflow-hidden rounded-lg border border-border bg-popover shadow-xl"
        onMouseDown={(e) => e.stopPropagation()}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="relative border-b border-border/60">
          <Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
          <Input
            ref={inputRef}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={handleInputKeyDown}
            placeholder="搜索笔记…"
            className="h-11 rounded-none border-0 bg-transparent pl-9 pr-3 focus-visible:ring-0 focus-visible:ring-offset-0"
          />
        </div>

        <ScrollArea className="max-h-[50vh]">
          {notes === null ? (
            <p className="px-4 py-10 text-center text-sm text-muted-foreground">加载中…</p>
          ) : rowCount === 0 ? (
            <p className="px-4 py-10 text-center text-sm text-muted-foreground">没有匹配的笔记</p>
          ) : (
            <ul className="p-1.5">
              {scored.map((item, idx) => {
                const isActive = idx === activeIndex;
                const n = item.note;
                return (
                  <li key={n.key}>
                    <button
                      type="button"
                      ref={(el) => {
                        if (el) itemRefs.current.set(idx, el);
                        else itemRefs.current.delete(idx);
                      }}
                      onMouseEnter={() => setActiveIndex(idx)}
                      onClick={() => handleSelect(n.key)}
                      className={`flex w-full flex-col gap-1 rounded-md px-3 py-2 text-left transition-colors ${isActive ? "bg-accent" : "hover:bg-accent/60"}`}
                    >
                      <span className="line-clamp-1 text-sm font-medium">
                        {item.winField === "title" && trimmed ? (
                          <TitleWithHighlight title={n.title} query={trimmed} />
                        ) : (
                          n.title
                        )}
                      </span>
                      <span className="truncate text-xs text-muted-foreground">{n.key}</span>
                      {n.tags.length > 0 && (
                        <span className="flex flex-wrap gap-1">
                          {n.tags.map((t) => (
                            <Badge key={t} variant="secondary" className="px-1.5 py-0 text-[10px]">
                              {t}
                            </Badge>
                          ))}
                        </span>
                      )}
                    </button>
                  </li>
                );
              })}
              {showCreate && (
                <li key="__create__">
                  <button
                    type="button"
                    ref={(el) => {
                      if (el) itemRefs.current.set(scored.length, el);
                      else itemRefs.current.delete(scored.length);
                    }}
                    onMouseEnter={() => setActiveIndex(scored.length)}
                    onClick={() => {
                      if (createState.kind === "ready") handleCreate(createState.key);
                    }}
                    disabled={createState.kind !== "ready"}
                    className={`flex w-full flex-col gap-1 rounded-md px-3 py-2 text-left transition-colors ${
                      activeIndex === scored.length ? "bg-accent" : "hover:bg-accent/60"
                    } ${createState.kind !== "ready" ? "cursor-not-allowed opacity-80" : ""}`}
                  >
                    <span className="flex items-center gap-1.5 text-sm font-medium">
                      <Plus className="size-4 shrink-0" />
                      <span className="truncate">创建笔记“{trimmed}”</span>
                    </span>
                    {createState.kind === "invalid" ? (
                      <span className="text-xs text-destructive">{createState.error}</span>
                    ) : (
                      <span className="text-xs text-muted-foreground">Enter 直接创建并打开</span>
                    )}
                  </button>
                </li>
              )}
            </ul>
          )}
        </ScrollArea>

        <div className="border-t border-border/60 px-3 py-1.5 text-xs text-muted-foreground">
          ↑↓ 选择 · Enter 打开/创建 · Esc 关闭
        </div>
      </div>
    </div>
  );

  return createPortal(overlay, document.body);
}
