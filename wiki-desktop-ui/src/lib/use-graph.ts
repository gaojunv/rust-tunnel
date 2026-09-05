/**
 * 共享图谱 hook —— 多面板同 refreshToken 复用一次 getGraph()
 * 模块级 Map<number, Promise<GraphDto>> 去重；新 token 时清旧缓存防泄漏
 */
import { useEffect, useState } from "react";
import { getGraph } from "@/api/tauri";
import type { GraphDto } from "@/api/types";

// 模块级缓存：refreshToken -> Promise
const cache = new Map<number, Promise<GraphDto>>();

export function useGraph(refreshToken: number): { graph: GraphDto | null; loading: boolean } {
  const [graph, setGraph] = useState<GraphDto | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);

    let promise = cache.get(refreshToken);
    if (!promise) {
      // 清旧缓存防泄漏：仅保留当前 token
      for (const k of [...cache.keys()]) {
        if (k !== refreshToken) cache.delete(k);
      }
      promise = getGraph();
      cache.set(refreshToken, promise);
      // 失败时清掉该条目，允许重试
      promise.catch(() => {
        if (cache.get(refreshToken) === promise) cache.delete(refreshToken);
      });
    }

    promise
      .then((g) => {
        if (!cancelled) {
          setGraph(g);
          setLoading(false);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setGraph(null);
          setLoading(false);
        }
      });

    return () => {
      cancelled = true;
    };
  }, [refreshToken]);

  return { graph, loading };
}
