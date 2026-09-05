/**
 * 反向链接面板 —— 展示「有哪些笔记链接到当前笔记」
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { Link2 } from "lucide-react";
import { getNote } from "@/api/tauri";
import { useGraph } from "@/lib/use-graph";

type Props = {
  selectedKey: string | null;
  refreshToken: number;
  onNavigate: (key: string) => void;
};

function snippetForBody(body: string, targetKey: string): string {
  const basename = targetKey.split("/").pop() ?? targetKey;
  // 找 [[targetKey]] 或 [[basename]] 或 [[targetKey|...]] 等
  const patterns = [targetKey, basename].filter(Boolean);
  let idx = -1;
  let hitLen = 0;
  for (const p of patterns) {
    const re = new RegExp(`\\[\\[${escapeRegExp(p)}(?:\\|[^\\]]+)?\\]\\]`);
    const m = re.exec(body);
    if (m && m.index !== -1) {
      idx = m.index;
      hitLen = m[0].length;
      break;
    }
  }
  if (idx !== -1) {
    const start = Math.max(0, idx - 60);
    const end = Math.min(body.length, idx + hitLen + 60);
    const prefix = start > 0 ? "…" : "";
    const suffix = end < body.length ? "…" : "";
    return prefix + body.slice(start, end).replace(/\s+/g, " ").trim() + suffix;
  }
  // 找不到链接文本就取开头 80 字符
  const head = body.slice(0, 80).replace(/\s+/g, " ").trim();
  return head.length < body.length ? head + "…" : head;
}

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

export function BacklinksPanel({ selectedKey, refreshToken, onNavigate }: Props) {
  const { graph, loading } = useGraph(refreshToken);
  const titleMap = useMemo(() => {
    if (!graph) return new Map<string, string>();
    return new Map(graph.nodes.map((n) => [n.key, n.title] as const));
  }, [graph]);

  const fromKeys = useMemo(() => {
    if (!graph || !selectedKey) return [];
    const seen = new Set<string>();
    const out: string[] = [];
    for (const e of graph.edges) {
      if (e.to === selectedKey && !seen.has(e.from)) {
        seen.add(e.from);
        out.push(e.from);
      }
    }
    return out;
  }, [graph, selectedKey]);

  // 懒加载：Map 缓存避免重复 getNote
  const cacheRef = useRef<Map<string, string>>(new Map());
  const [snippets, setSnippets] = useState<Map<string, string>>(new Map());

  useEffect(() => {
    if (fromKeys.length === 0) {
      setSnippets(new Map());
      return;
    }
    let cancelled = false;
    const toFetch = fromKeys.filter((k) => !cacheRef.current.has(k));
    if (toFetch.length === 0) {
      setSnippets(new Map(cacheRef.current));
      return;
    }
    Promise.all(
      toFetch.map(async (k) => {
        try {
          const note = await getNote(k);
          const snippet = snippetForBody(note.body, selectedKey!);
          cacheRef.current.set(k, snippet);
        } catch {
          cacheRef.current.set(k, "");
        }
      }),
    ).then(() => {
      if (!cancelled) setSnippets(new Map(cacheRef.current));
    });
    return () => {
      cancelled = true;
    };
  }, [fromKeys, selectedKey]);

  // 切换笔记时若已有缓存，同步显示
  useEffect(() => {
    if (fromKeys.length > 0) {
      const cached = fromKeys.filter((k) => cacheRef.current.has(k));
      if (cached.length > 0) setSnippets(new Map(cacheRef.current));
    }
  }, [fromKeys]);

  if (!selectedKey) {
    return <p className="p-4 text-sm text-muted-foreground">选中一篇笔记后，这里会展示反向链接。</p>;
  }
  if (loading && !graph) {
    return <p className="p-4 text-sm text-muted-foreground">加载中…</p>;
  }
  if (fromKeys.length === 0) {
    return <p className="p-4 text-sm text-muted-foreground">没有笔记链接到这里</p>;
  }

  return (
    <div className="flex flex-col gap-2 p-3">
      {fromKeys.map((k) => {
        const title = titleMap.get(k) ?? k;
        const snippet = snippets.get(k) ?? "";
        return (
          <button
            key={k}
            type="button"
            onClick={() => onNavigate(k)}
            className="rounded-md border px-3 py-2.5 text-left hover:bg-accent/60"
          >
            <span className="flex items-center gap-1.5 text-sm font-medium">
              <Link2 className="size-3.5 shrink-0 text-muted-foreground" />
              {title}
            </span>
            <span className="mt-1 block text-xs text-muted-foreground">{k}</span>
            {snippet && <span className="mt-1.5 block line-clamp-2 text-xs leading-5 text-muted-foreground">{snippet}</span>}
          </button>
        );
      })}
    </div>
  );
}
