// Global SSE singleton for AI memory distill/inject status.
// Backend pushes MemoryEvent on the "memory" event name (plus "ping"
// keep-alives which callers don't need). Mirrors knowledgeStream.ts.
import type { MemoryEvent } from '@/types';

type Callback = (e: MemoryEvent) => void;

/** SSE 重连指数退避：1s → 2s → 4s → …，上限 30s。连接建立成功（onopen，HTTP
 *  200 后触发）即重置为初始值——memory 流在无蒸馏/注入任务时可能长时间无事件，
 *  onopen 比「收到首事件」更可靠地代表连接已恢复。 */
const INITIAL_RETRY_MS = 1000;
const MAX_RETRY_MS = 30_000;

class MemoryStream {
  private es: EventSource | null = null;
  private listeners = new Set<Callback>();
  private retryDelayMs = INITIAL_RETRY_MS;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private static instance: MemoryStream;

  static getInstance(): MemoryStream {
    if (!MemoryStream.instance) {
      MemoryStream.instance = new MemoryStream();
    }
    return MemoryStream.instance;
  }

  private connect(): void {
    // 清除在途重连定时器：subscribe 在退避等待窗口内直接建连时，避免定时器
    // 到点后二次 connect 产生两条 EventSource 双发事件
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    const token = localStorage.getItem('auth_token') || '';
    this.es = new EventSource(`/api/agent/memory/events?token=${encodeURIComponent(token)}`);
    this.es.onopen = () => {
      this.retryDelayMs = INITIAL_RETRY_MS;
    };
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
        const delay = this.retryDelayMs;
        this.retryDelayMs = Math.min(this.retryDelayMs * 2, MAX_RETRY_MS);
        this.reconnectTimer = setTimeout(() => this.connect(), delay);
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
