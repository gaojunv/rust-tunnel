// Global SSE singleton for wiki document ingestion status.
// Backend pushes WikiEvent on the "wiki" event name, a "sync" event when the
// broadcast lagged (buffer overflow → events dropped, caller should re-fetch),
// and "ping" keep-alives (callers don't need). Mirrors memoryStream.ts.
import type { WikiEvent } from '@/types';
import { createSseStream } from './sseStream';

export interface WikiStreamHandlers {
  /** 正常摄入状态事件。 */
  onWiki: (e: WikiEvent) => void;
  /** broadcast 槽位 Lagged：期间可能丢事件，调用方应重拉列表。 */
  onSync?: (lagged: number) => void;
}

const inner = createSseStream<{ wiki: WikiEvent; sync: number }>({
  url: '/api/agent/wiki/events',
  parsers: {
    wiki: (raw) => JSON.parse(raw) as WikiEvent,
    sync: (raw) => {
      const parsed = JSON.parse(raw) as { lagged?: number };
      return parsed.lagged ?? 0;
    },
  },
});

export const wikiStream = {
  subscribe(handlers: WikiStreamHandlers): () => void {
    const mapped: { wiki: (e: WikiEvent) => void; sync?: (lagged: number) => void } = {
      wiki: handlers.onWiki,
    };
    if (handlers.onSync) mapped.sync = handlers.onSync;
    return inner.subscribe(mapped);
  },
};
