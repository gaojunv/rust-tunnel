// Global SSE singleton for wiki document ingestion status.
// Backend pushes WikiEvent on the "wiki" event name, a "sync" event when the
// broadcast lagged (buffer overflow → events dropped, caller should re-fetch),
// and "ping" keep-alives (callers don't need). Mirrors memoryStream.ts.
import type { WikiEvent } from '@/types';

export interface WikiStreamHandlers {
  /** 正常摄入状态事件。 */
  onWiki: (e: WikiEvent) => void;
  /** broadcast 槽位 Lagged：期间可能丢事件，调用方应重拉列表。 */
  onSync?: (lagged: number) => void;
}

/** SSE 重连指数退避：1s → 2s → 4s → …，上限 30s。连接建立成功（onopen，HTTP
 *  200 后触发）即重置为初始值——wiki 流在无摄入任务时可能长时间无事件，
 *  onopen 比「收到首事件」更可靠地代表连接已恢复。 */
const INITIAL_RETRY_MS = 1000;
const MAX_RETRY_MS = 30_000;

class WikiStream {
  private es: EventSource | null = null;
  private listeners = new Set<WikiStreamHandlers>();
  private retryDelayMs = INITIAL_RETRY_MS;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private static instance: WikiStream;

  static getInstance(): WikiStream {
    if (!WikiStream.instance) {
      WikiStream.instance = new WikiStream();
    }
    return WikiStream.instance;
  }

  private connect(): void {
    // 清除在途重连定时器：subscribe 在退避等待窗口内直接建连时，避免定时器
    // 到点后二次 connect 产生两条 EventSource 双发事件
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    const token = localStorage.getItem('auth_token') || '';
    this.es = new EventSource(`/api/agent/wiki/events?token=${encodeURIComponent(token)}`);
    this.es.onopen = () => {
      this.retryDelayMs = INITIAL_RETRY_MS;
    };
    this.es.addEventListener('wiki', (e: MessageEvent) => {
      try {
        const event: WikiEvent = JSON.parse(e.data);
        this.listeners.forEach((cb) => cb.onWiki(event));
      } catch {
        // ignore parse errors
      }
    });
    this.es.addEventListener('sync', (e: MessageEvent) => {
      try {
        const parsed = JSON.parse(e.data) as { lagged?: number };
        const lagged = parsed.lagged ?? 0;
        this.listeners.forEach((cb) => cb.onSync?.(lagged));
      } catch {
        // ignore parse errors
      }
    });
    this.es.onerror = () => {
      this.es?.close();
      this.es = null;
      // 重连前确认还有订阅者，避免最后一个 unsub 后仍建立无监听者连接
      if (this.listeners.size > 0) {
        const delay = this.retryDelayMs;
        this.retryDelayMs = Math.min(this.retryDelayMs * 2, MAX_RETRY_MS);
        this.reconnectTimer = setTimeout(() => this.connect(), delay);
      }
    };
  }

  subscribe(handlers: WikiStreamHandlers): () => void {
    this.listeners.add(handlers);
    if (!this.es || this.es.readyState === EventSource.CLOSED) {
      this.connect();
    }
    return () => {
      this.listeners.delete(handlers);
      if (this.listeners.size === 0) {
        this.es?.close();
        this.es = null;
      }
    };
  }
}

export const wikiStream = WikiStream.getInstance();
