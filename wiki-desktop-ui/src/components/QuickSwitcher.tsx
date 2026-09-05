import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Search } from "lucide-react";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { ScrollArea } from "@/components/ui/scroll-area";
import { listNotes } from "@/api/tauri";
import type { NoteSummary } from "@/api/types";
import { fuzzyScore, matchIndices } from "@/lib/fuzzy";

type Props = {
  open: boolean;
  onClose: () => void;
  onSelect: (key: string) => void;
};

type Scored = {
  note: NoteSummary;
  score: number;
  winField: "title" | "key" | "tags" | null;
};

function TitleWithHighlight({ title, query }: { title: string; query: string }) {
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
}

export function QuickSwitcher({ open, onClose, onSelect }: Props) {
  const [query, setQuery] = useState("");
  const [notes, setNotes] = useState<NoteSummary[] | null>(null);
  const [activeIndex, setActiveIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const itemRefs = useRef<Map<number, HTMLButtonElement>>(new Map());
  const trimmed = query.trim();

  // 每次打开：重置状态、聚焦、拉取最新列表
  useEffect(() => {
    if (!open) return;
    setQuery("");
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

  const scored: Scored[] = useMemo(() => {
    if (notes === null) return [];
    if (!trimmed) {
      return [...notes]
        .sort((a, b) => b.modified - a.modified)
        .slice(0, 15)
        .map((n) => ({ note: n, score: 0, winField: null }));
    }
    const out: Scored[] = [];
    for (const n of notes) {
      const sTitle = fuzzyScore(n.title, trimmed);
      const sKey = fuzzyScore(n.key, trimmed);
      const sTags = fuzzyScore(n.tags.join(" "), trimmed);
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
      if (best !== null) out.push({ note: n, score: best, winField: win });
    }
    out.sort((a, b) => {
      if (b.score !== a.score) return b.score - a.score;
      return b.note.modified - a.note.modified;
    });
    return out.slice(0, 15);
  }, [notes, trimmed]);

  // 结果变化时把 activeIndex 约束回合法区间
  useEffect(() => {
    if (scored.length === 0) {
      setActiveIndex(0);
      return;
    }
    if (activeIndex >= scored.length) setActiveIndex(0);
  }, [scored, activeIndex]);

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

  const handleInputKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLInputElement>) => {
      if (scored.length === 0 && e.key !== "Escape") return;
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setActiveIndex((i) => (i + 1) % scored.length);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setActiveIndex((i) => (i - 1 + scored.length) % scored.length);
      } else if (e.key === "Enter") {
        e.preventDefault();
        const cur = scored[activeIndex];
        if (cur) handleSelect(cur.note.key);
      } else if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    },
    [scored, activeIndex, handleSelect, onClose],
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
          ) : scored.length === 0 ? (
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
            </ul>
          )}
        </ScrollArea>

        <div className="border-t border-border/60 px-3 py-1.5 text-xs text-muted-foreground">
          ↑↓ 选择 · Enter 打开 · Esc 关闭
        </div>
      </div>
    </div>
  );

  return createPortal(overlay, document.body);
}
