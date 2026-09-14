/**
 * 大纲 TOC 面板
 * - noteKey/refreshToken 变化时 getCurrentNote -> extractToc（落盘兜底）
 * - 编辑中：订阅 note-live-content 实时快照（debounce 200ms），extractToc 重算 + 光标行高亮
 * - 点击跳转：复用 onScrollToLine
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { List } from "lucide-react";
import { extractToc, resolveActiveTocIndex, type TocItem } from "@/lib/markdown-toc";
import {
  getNoteLiveSnapshot,
  subscribeNoteLiveContent,
} from "@/lib/note-live-content";
import { cn } from "@/lib/utils";
import type { NoteDto } from "@/api/types";

type Props = {
  noteKey: string | null;
  getCurrentNote: () => Promise<NoteDto | null>;
  refreshToken: number;
  onScrollToLine: (line: number) => void;
};

export function TocPanel({ noteKey, getCurrentNote, refreshToken, onScrollToLine }: Props) {
  const [items, setItems] = useState<TocItem[]>([]);
  const [cursorLine, setCursorLine] = useState<number | null>(null);
  const [loading, setLoading] = useState(false);
  const activeItemRef = useRef<HTMLButtonElement | null>(null);

  // 落盘兜底：noteKey/refreshToken 变化时从后端拉取；
  // 已有实时快照（编辑中）则直接采用，跳过拉取，避免旧内容覆盖与 loading 闪烁
  useEffect(() => {
    if (!noteKey) {
      setItems([]);
      setCursorLine(null);
      return;
    }
    const live = getNoteLiveSnapshot(noteKey);
    if (live) {
      setItems(extractToc(live.body));
      setCursorLine(live.cursorLine);
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    getCurrentNote()
      .then((note) => {
        if (cancelled) return;
        if (!note) {
          setItems([]);
          return;
        }
        setItems(extractToc(note.body));
      })
      .catch(() => {
        if (!cancelled) setItems([]);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [noteKey, refreshToken, getCurrentNote]);

  // 实时订阅：编辑中文档变化 -> 重算 TOC + 更新光标行
  useEffect(() => {
    if (!noteKey) return;
    // 挂载时若已有快照（编辑器先发布），直接应用
    const initial = getNoteLiveSnapshot(noteKey);
    if (initial) {
      setItems(extractToc(initial.body));
      setCursorLine(initial.cursorLine);
    }
    const unsub = subscribeNoteLiveContent((snapshot) => {
      if (snapshot.noteKey !== noteKey) return;
      setItems(extractToc(snapshot.body));
      setCursorLine(snapshot.cursorLine);
    });
    return unsub;
  }, [noteKey]);

  // 高亮项进入视口（仅当用户未在 TOC 内手动滚动时也无妨：block nearest 开销小）
  const activeIndex = useMemo(() => resolveActiveTocIndex(items, cursorLine), [items, cursorLine]);
  useEffect(() => {
    activeItemRef.current?.scrollIntoView({ block: "nearest" });
  }, [activeIndex, noteKey]);

  const handleClick = (item: TocItem) => {
    onScrollToLine(item.line);
  };

  if (!noteKey) {
    return <p className="p-4 text-sm text-muted-foreground">选中一篇笔记后，这里会展示大纲。</p>;
  }
  if (loading) {
    return <p className="p-4 text-sm text-muted-foreground">加载中…</p>;
  }
  if (items.length === 0) {
    return <p className="p-4 text-sm text-muted-foreground">该笔记没有标题</p>;
  }

  return (
    <div className="p-2">
      <div className="mb-2 flex items-center gap-1.5 px-2 text-xs font-medium text-muted-foreground">
        <List className="size-3.5" />
        大纲
      </div>
      <ul className="space-y-0.5">
        {items.map((item, idx) => {
          const active = idx === activeIndex;
          return (
            <li key={`${item.line}-${idx}`}>
              <button
                type="button"
                ref={active ? activeItemRef : undefined}
                onClick={() => handleClick(item)}
                aria-current={active ? "true" : undefined}
                className={cn(
                  "w-full rounded px-2 py-1 text-left text-xs transition-colors hover:bg-accent/60",
                  active && "bg-accent font-medium text-accent-foreground",
                )}
                style={{ paddingLeft: 8 + (item.level - 1) * 12 }}
                title={item.text}
              >
                <span className="line-clamp-1">{item.text}</span>
              </button>
            </li>
          );
        })}
      </ul>
    </div>
  );
}
