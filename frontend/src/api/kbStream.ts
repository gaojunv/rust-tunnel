// Global SSE singleton for knowledge base document ingestion status.
// Backend pushes KbEvent on the "kb" event name (plus "sync" lagged notices
// and "ping" keep-alives which callers don't need). Mirrors statsStream.ts.
import type { KbEvent } from '@/types';
import { createSseStream } from './sseStream';

type Callback = (e: KbEvent) => void;

const inner = createSseStream<{ kb: KbEvent }>({
  url: '/api/llm/kb/events',
  parsers: {
    kb: (raw) => JSON.parse(raw) as KbEvent,
  },
});

export const kbStream = {
  subscribe(callback: Callback): () => void {
    return inner.subscribe({ kb: callback });
  },
};
