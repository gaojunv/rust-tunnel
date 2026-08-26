// Shared SSE factory — extracts the identical reconnect skeleton from
// knowledgeStream.ts (memoryStream.ts mirrors the same shape).
// Keeps: per-stream singleton via module scope, lazy connect, exponential
// backoff (1s → 2s → … capped 30s), size===0 close, `?token=` auth query
// (copied verbatim from the original streams).

const INITIAL_RETRY_MS = 1000;
const MAX_RETRY_MS = 30_000;

export type SseParsers<M extends Record<string, unknown>> = {
  [K in keyof M]: (raw: string) => M[K];
};

export interface SseStream<M extends Record<string, unknown>> {
  subscribe(handlers: Partial<{ [K in keyof M]: (payload: M[K]) => void }>): () => void;
}

export function createSseStream<M extends Record<string, unknown>>(opts: {
  url: string;
  parsers: SseParsers<M>;
}): SseStream<M> {
  let es: EventSource | null = null;
  const listeners = new Set<Partial<{ [K in keyof M]: (payload: M[K]) => void }>>();
  let retryDelayMs = INITIAL_RETRY_MS;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

  const connect = (): void => {
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    const token = localStorage.getItem('auth_token') || '';
    const url = `${opts.url}?token=${encodeURIComponent(token)}`;
    const source = new EventSource(url);
    es = source;
    source.onopen = () => {
      retryDelayMs = INITIAL_RETRY_MS;
    };
    (Object.keys(opts.parsers) as (keyof M & string)[]).forEach((eventName) => {
      const parser = opts.parsers[eventName];
      source.addEventListener(eventName, (e: MessageEvent) => {
        try {
          const payload = parser(e.data);
          listeners.forEach((h) => {
            const cb = h[eventName];
            if (cb) {
              (cb as (p: M[typeof eventName]) => void)(payload);
            }
          });
        } catch {
          // ignore parse errors (mirrors original streams)
        }
      });
    });
    source.onerror = () => {
      source.close();
      es = null;
      if (listeners.size > 0) {
        const delay = retryDelayMs;
        retryDelayMs = Math.min(retryDelayMs * 2, MAX_RETRY_MS);
        reconnectTimer = setTimeout(() => connect(), delay);
      }
    };
  };

  return {
    subscribe(handlers: Partial<{ [K in keyof M]: (payload: M[K]) => void }>): () => void {
      listeners.add(handlers);
      if (!es || es.readyState === EventSource.CLOSED) {
        connect();
      }
      return () => {
        listeners.delete(handlers);
        if (listeners.size === 0) {
          es?.close();
          es = null;
        }
      };
    },
  };
}
