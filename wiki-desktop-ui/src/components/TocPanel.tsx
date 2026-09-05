/**
 * 大纲 TOC 面板
 * - noteKey/refreshToken 变化时 getCurrentNote -> extractToc
 * - 编辑态正文未保存的改动不要求实时（保存后刷新即可）
 */
import { useEffect, useState } from "react";
import { List } from "lucide-react";
import { extractToc, type TocItem } from "@/lib/markdown-toc";
import type { NoteDto } from "@/api/types";

type Props = {
  noteKey: string | null;
  getCurrentNote: () => Promise<NoteDto | null>;
  refreshToken: number;
  mode: "edit" | "preview";
  onScrollToLine: (line: number) => void;
  previewContainerRef: React.RefObject<HTMLElement | null>;
};

export function TocPanel({ noteKey, getCurrentNote, refreshToken, mode, onScrollToLine, previewContainerRef }: Props) {
  const [items, setItems] = useState<TocItem[]>([]);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!noteKey) {
      setItems([]);
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

  const handleClick = (item: TocItem, tocIndex: number) => {
    if (mode === "preview") {
      const container = previewContainerRef.current;
      if (container) {
        const headings = container.querySelectorAll("h1,h2,h3,h4,h5,h6");
        const target = headings[tocIndex] as HTMLElement | undefined;
        if (target) {
          target.scrollIntoView({ behavior: "smooth", block: "start" });
          return;
        }
      }
      // 兜底：仍尝试按行跳转（若容器未挂载）
      onScrollToLine(item.line);
    } else {
      onScrollToLine(item.line);
    }
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
        {items.map((item, idx) => (
          <li key={`${item.line}-${idx}`}>
            <button
              type="button"
              onClick={() => handleClick(item, idx)}
              className="w-full rounded px-2 py-1 text-left text-xs hover:bg-accent/60"
              style={{ paddingLeft: 8 + (item.level - 1) * 12 }}
              title={item.text}
            >
              <span className="line-clamp-1">{item.text}</span>
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}
