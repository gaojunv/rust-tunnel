// Global SSE singleton for unified stats stream
import type { StatsSnapshot } from '@/types';

type Callback = (snapshot: StatsSnapshot) => void;

/** SSE 重连指数退避：1s → 2s → 4s → …，上限 30s。连接建立成功（onopen，HTTP
 *  200 后触发）即重置为初始值。 */
const INITIAL_RETRY_MS = 1000;
const MAX_RETRY_MS = 30_000;

class StatsStream {
  private es: EventSource | null = null;
  private listeners = new Map<string, Set<Callback>>();
  private retryDelayMs = INITIAL_RETRY_MS;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private static instance: StatsStream;

  static getInstance(): StatsStream {
    if (!StatsStream.instance) {
      StatsStream.instance = new StatsStream();
    }
    return StatsStream.instance;
  }

  private connect(entityType?: string): void {
    // 清除在途重连定时器：subscribe 在退避等待窗口内直接建连时，避免定时器
    // 到点后二次 connect 产生两条 EventSource 双发事件
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    const token = localStorage.getItem('auth_token') || '';
    let url = `/api/stats/stream?token=${encodeURIComponent(token)}`;
    if (entityType) {
      url += `&entity_type=${encodeURIComponent(entityType)}`;
    }
    this.es = new EventSource(url);
    this.es.onopen = () => {
      this.retryDelayMs = INITIAL_RETRY_MS;
    };
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
      const delay = this.retryDelayMs;
      this.retryDelayMs = Math.min(this.retryDelayMs * 2, MAX_RETRY_MS);
      this.reconnectTimer = setTimeout(() => {
        this.reconnectTimer = null;
        // 重连前确认还有订阅者，避免最后一个 unsub 后仍建立无监听者连接
        const hasListeners = [...this.listeners.values()].some((s) => s.size > 0);
        if (hasListeners) {
          this.connect(entityType);
        }
      }, delay);
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
