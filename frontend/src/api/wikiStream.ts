// Global SSE singleton for document ingestion status.
// Backend pushes KbEvent on the "knowledge" event name (plus "sync" lagged
// notices and "ping" keep-alives which callers don't need). Unified endpoint
// carries both vector and pages events; this stream filters for pages.
import type { KbEvent } from '@/types';
import { createSseStream } from './sseStream';

export interface WikiStreamHandlers {
  /** 正常摄入状态事件（映射回旧 WikiEvent 形）。 */
  onWiki: (e: { wiki_id: string; doc_id: string; status: string; page_count: number; error?: string | null }) => void;
  /** broadcast 槽位 Lagged：期间可能丢事件，调用方应重拉列表。 */
  onSync?: (lagged: number) => void;
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

export const wikiStream = {
  subscribe(handlers: WikiStreamHandlers): () => void {
    return inner.subscribe({
      knowledge: (ev) => {
        if (ev.kind !== 'pages') return;
        handlers.onWiki({
          wiki_id: ev.kb_id,
          doc_id: ev.doc_id,
          status: ev.status,
          page_count: ev.chunk_count,
          error: ev.error,
        });
      },
      sync: (lagged) => handlers.onSync?.(lagged),
    });
  },
};
