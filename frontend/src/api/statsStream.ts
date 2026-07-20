// Global SSE singleton for unified stats stream
import type { StatsSnapshot } from '@/types';

type Callback = (snapshot: StatsSnapshot) => void;

class StatsStream {
  private es: EventSource | null = null;
  private listeners = new Map<string, Set<Callback>>();
  private static instance: StatsStream;

  static getInstance(): StatsStream {
    if (!StatsStream.instance) {
      StatsStream.instance = new StatsStream();
    }
    return StatsStream.instance;
  }

  private connect(entityType?: string): void {
    const token = localStorage.getItem('token') || '';
    let url = `/api/stats/stream?token=${encodeURIComponent(token)}`;
    if (entityType) {
      url += `&entity_type=${encodeURIComponent(entityType)}`;
    }
    this.es = new EventSource(url);
    this.es.addEventListener('snapshot', (e: MessageEvent) => {
      try {
        const snap: StatsSnapshot = JSON.parse(e.data);
        ['*', snap.entity_type].forEach((key) => {
          this.listeners.get(key)?.forEach((cb) => cb(snap));
        });
      } catch {
        // ignore parse errors
      }
    });
    this.es.onerror = () => {
      this.es?.close();
      this.es = null;
      setTimeout(() => this.connect(entityType), 3000);
    };
  }

  subscribe(entityType: string | undefined, callback: Callback): () => void {
    const key = entityType ?? '*';
    if (!this.listeners.has(key)) {
      this.listeners.set(key, new Set());
    }
    this.listeners.get(key)!.add(callback);

    if (!this.es || this.es.readyState === EventSource.CLOSED) {
      this.connect(entityType);
    }

    return () => {
      this.listeners.get(key)?.delete(callback);
      if ([...this.listeners.values()].every((s) => s.size === 0)) {
        this.es?.close();
        this.es = null;
      }
    };
  }
}

export const statsStream = StatsStream.getInstance();
