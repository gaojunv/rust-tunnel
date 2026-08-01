// Global SSE singleton for knowledge base document ingestion status.
// Backend pushes KbEvent on the "kb" event name (plus "sync" lagged notices
// and "ping" keep-alives which callers don't need). Mirrors statsStream.ts.
import type { KbEvent } from '@/types';

type Callback = (e: KbEvent) => void;

class KbStream {
  private es: EventSource | null = null;
  private listeners = new Set<Callback>();
  private static instance: KbStream;

  static getInstance(): KbStream {
    if (!KbStream.instance) {
      KbStream.instance = new KbStream();
    }
    return KbStream.instance;
  }

  private connect(): void {
    const token = localStorage.getItem('auth_token') || '';
    this.es = new EventSource(`/api/llm/kb/events?token=${encodeURIComponent(token)}`);
    this.es.addEventListener('kb', (e: MessageEvent) => {
      try {
        const event: KbEvent = JSON.parse(e.data);
        this.listeners.forEach((cb) => cb(event));
      } catch {
        // ignore parse errors
      }
    });
    this.es.onerror = () => {
      this.es?.close();
      this.es = null;
      // 重连前确认还有订阅者，避免最后一个 unsub 后仍建立无监听者连接
      if (this.listeners.size > 0) {
        setTimeout(() => this.connect(), 3000);
      }
    };
  }

  subscribe(callback: Callback): () => void {
    this.listeners.add(callback);
    if (!this.es || this.es.readyState === EventSource.CLOSED) {
      this.connect();
    }
    return () => {
      this.listeners.delete(callback);
      if (this.listeners.size === 0) {
        this.es?.close();
        this.es = null;
      }
    };
  }
}

export const kbStream = KbStream.getInstance();
