// Global SSE singleton for AI memory distill/inject status.
// Backend pushes MemoryEvent on the "memory" event name (plus "ping"
// keep-alives which callers don't need). Mirrors kbStream.ts.
import type { MemoryEvent } from '@/types';

type Callback = (e: MemoryEvent) => void;

class MemoryStream {
  private es: EventSource | null = null;
  private listeners = new Set<Callback>();
  private static instance: MemoryStream;

  static getInstance(): MemoryStream {
    if (!MemoryStream.instance) {
      MemoryStream.instance = new MemoryStream();
    }
    return MemoryStream.instance;
  }

  private connect(): void {
    const token = localStorage.getItem('auth_token') || '';
    this.es = new EventSource(`/api/agent/memory/events?token=${encodeURIComponent(token)}`);
    this.es.addEventListener('memory', (e: MessageEvent) => {
      try {
        const event: MemoryEvent = JSON.parse(e.data);
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

export const memoryStream = MemoryStream.getInstance();
