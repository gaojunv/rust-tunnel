// 统一知识容器摄入状态 SSE 单例。
//
// 后端在 `knowledge` 事件名上推 KbEvent（另有 `sync` 表示 broadcast 槽位 Lagged、
// `ping` 保活，调用方无需关心 ping）。同一条流承载 vector 与 pages 两侧事件，
// 由 `kind` 字段区分；调用方可传 `kind` 只订阅一侧，缺省收两侧。
import type { KbEvent, KnowledgeIndexKind } from '@/types';
import { createSseStream } from './sseStream';

export interface KnowledgeStreamHandlers {
  /** 摄入状态事件。`chunk_count` 在 pages 侧语义为页数。 */
  onEvent: (e: KbEvent) => void;
  /** broadcast 槽位 Lagged：期间可能丢事件，调用方应重拉列表。 */
  onSync?: (lagged: number) => void;
  /** 只订阅该索引侧；缺省收两侧。 */
  kind?: KnowledgeIndexKind;
}

const inner = createSseStream<{ knowledge: KbEvent; sync: number }>({
  url: '/api/knowledge/events',
  parsers: {
    knowledge: (raw) => JSON.parse(raw) as KbEvent,
    sync: (raw) => {
      const parsed = JSON.parse(raw) as { lagged?: number };
      return parsed.lagged ?? 0;
    },
  },
});

export const knowledgeStream = {
  subscribe(handlers: KnowledgeStreamHandlers): () => void {
    return inner.subscribe({
      knowledge: (ev) => {
        if (handlers.kind && ev.kind !== handlers.kind) return;
        handlers.onEvent(ev);
      },
      sync: (lagged) => handlers.onSync?.(lagged),
    });
  },
};
