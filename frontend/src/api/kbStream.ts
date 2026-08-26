// Global SSE singleton for knowledge base document ingestion status.
// Backend pushes KbEvent on the "knowledge" event name (plus "sync" lagged
// notices and "ping" keep-alives which callers don't need). Unified endpoint
// carries both vector and pages events; this stream filters for vector.
import type { KbEvent } from '@/types';
import { createSseStream } from './sseStream';

type Callback = (e: KbEvent) => void;

const inner = createSseStream<{ knowledge: KbEvent }>({
  url: '/api/knowledge/events',
  parsers: {
    knowledge: (raw) => JSON.parse(raw) as KbEvent,
  },
});

export const kbStream = {
  subscribe(callback: Callback): () => void {
    return inner.subscribe({
      knowledge: (ev) => {
        if (ev.kind === 'vector') callback(ev);
      },
    });
  },
};
