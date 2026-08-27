import { useEffect, useMemo, useState } from "react";
import { Network, Link2, Unlink, AlertTriangle } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { getGraph, getNote } from "@/api/tauri";
import type { GraphDto } from "@/api/types";

type Props = {
  selectedKey: string | null;
  refreshToken: number;
};

function extractLinks(body: string): string[] {
  const re = /\[\[([^\]|]+)(?:\|[^\]]+)?\]\]/g;
  const out: string[] = [];
  let m: RegExpExecArray | null;
  while ((m = re.exec(body))) out.push(m[1].trim());
  return out;
}

export function GraphPanel({ selectedKey, refreshToken }: Props) {
  const [graph, setGraph] = useState<GraphDto | null>(null);
  const [body, setBody] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    getGraph()
      .then((g) => {
        if (!cancelled) setGraph(g);
      })
      .catch(() => {
        if (!cancelled) setGraph(null);
      });
    return () => {
      cancelled = true;
    };
  }, [refreshToken]);

  useEffect(() => {
    if (!selectedKey) {
      setBody(null);
      return;
    }
    let cancelled = false;
    getNote(selectedKey)
      .then((n) => {
        if (!cancelled) setBody(n?.body ?? null);
      })
      .catch(() => {
        if (!cancelled) setBody(null);
      });
    return () => {
      cancelled = true;
    };
  }, [selectedKey]);

  const derived = useMemo(() => {
    if (!graph || !selectedKey) return null;
    const keys = new Set(graph.nodes.map((n) => n.key));
    const outgoing = graph.edges.filter((e) => e.from === selectedKey).map((e) => e.to);
    const backlinks = graph.edges.filter((e) => e.to === selectedKey).map((e) => e.from);
    const rawLinks = body ? extractLinks(body) : [];
    const broken = rawLinks.filter((k) => !keys.has(k));
    // 去重
    const uniq = (arr: string[]) => [...new Set(arr)];
    return {
      outgoing: uniq(outgoing),
      backlinks: uniq(backlinks),
      broken: uniq(broken),
      isolated: outgoing.length === 0 && backlinks.length === 0,
    };
  }, [graph, selectedKey, body]);

  // 全局统计（不管是否选中）
  const globalStats = useMemo(() => {
    if (!graph) return null;
    const edgeCount = graph.edges.length;
    const nodeCount = graph.nodes.length;
    // 入度为 0 的孤儿
    const inDegree = new Map<string, number>();
    for (const n of graph.nodes) inDegree.set(n.key, 0);
    for (const e of graph.edges) inDegree.set(e.to, (inDegree.get(e.to) ?? 0) + 1);
    const orphans = [...inDegree.entries()].filter(([, d]) => d === 0).map(([k]) => k);
    return { nodeCount, edgeCount, orphans };
  }, [graph]);

  return (
    <div className="flex h-full flex-col gap-4 p-4">
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="flex items-center gap-2 text-sm">
            <Network className="size-4" />
            图谱（占位）
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-3 text-sm">
          {/* 后续会换成真实力导向图：此处仅文字列出统计，保留扩展点 */}
          <p className="text-xs text-muted-foreground">
            后续会替换为力导向图可视化。当前为占位实现：基于 <code className="rounded bg-muted px-1">get_graph</code> 推导
            出链 / 反链 / 断链。
          </p>

          {globalStats && (
            <div className="flex flex-wrap gap-2 text-xs">
              <Badge variant="secondary">{globalStats.nodeCount} 节点</Badge>
              <Badge variant="secondary">{globalStats.edgeCount} 条边</Badge>
              <Badge variant="outline">{globalStats.orphans.length} 孤儿</Badge>
            </div>
          )}

          {!selectedKey ? (
            <p className="text-sm text-muted-foreground">选中一篇笔记后，这里会展示它的出链 / 反链 / 断链。</p>
          ) : !derived ? (
            <p className="text-sm text-muted-foreground">加载中…</p>
          ) : (
            <div className="space-y-4">
              <section>
                <h4 className="mb-1.5 flex items-center gap-1.5 text-sm font-medium">
                  <Link2 className="size-3.5" />
                  出链 ({derived.outgoing.length})
                </h4>
                {derived.outgoing.length === 0 ? (
                  <p className="text-xs text-muted-foreground">无出链</p>
                ) : (
                  <ul className="space-y-1">
                    {derived.outgoing.map((k) => (
                      <li key={k} className="rounded bg-muted px-2 py-1 text-xs">
                        {k}
                      </li>
                    ))}
                  </ul>
                )}
              </section>

              <section>
                <h4 className="mb-1.5 flex items-center gap-1.5 text-sm font-medium">
                  <Unlink className="size-3.5" />
                  反链 ({derived.backlinks.length})
                </h4>
                {derived.backlinks.length === 0 ? (
                  <p className="text-xs text-muted-foreground">无反链</p>
                ) : (
                  <ul className="space-y-1">
                    {derived.backlinks.map((k) => (
                      <li key={k} className="rounded bg-muted px-2 py-1 text-xs">
                        {k}
                      </li>
                    ))}
                  </ul>
                )}
              </section>

              <section>
                <h4 className="mb-1.5 flex items-center gap-1.5 text-sm font-medium">
                  <AlertTriangle className="size-3.5 text-amber-600" />
                  断链 ({derived.broken.length})
                </h4>
                {derived.broken.length === 0 ? (
                  <p className="text-xs text-muted-foreground">无断链</p>
                ) : (
                  <ul className="space-y-1">
                    {derived.broken.map((k) => (
                      <li key={k} className="rounded bg-amber-50 px-2 py-1 text-xs text-amber-800 dark:bg-amber-950/40 dark:text-amber-200">
                        {k}
                      </li>
                    ))}
                  </ul>
                )}
              </section>

              {derived.isolated && <p className="text-xs text-muted-foreground">该笔记为孤儿节点（无出链也无反链）。</p>}
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
