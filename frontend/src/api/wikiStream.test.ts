// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { WikiEvent } from '@/types';

/** 可驱动的 EventSource 替身：记录实例、暴露 dispatch 触发自定义事件。 */
class MockEventSource {
  static instances: MockEventSource[] = [];
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSED = 2;

  url: string;
  readyState = 0;
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;
  private listeners = new Map<string, Set<(e: MessageEvent) => void>>();

  constructor(url: string) {
    this.url = url;
    MockEventSource.instances.push(this);
  }

  addEventListener(type: string, cb: (e: MessageEvent) => void): void {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type)!.add(cb);
  }

  /** 测试驱动：模拟服务端推送一条具名事件。 */
  dispatch(type: string, data: string): void {
    const listeners = this.listeners.get(type);
    if (!listeners) return;
    listeners.forEach((cb) => cb({ data } as MessageEvent));
  }

  close(): void {
    this.readyState = 2;
  }
}

/** 每次用例前把全局 EventSource 替换为替身；每个 describe 用 fresh import 保证
 *  单例全新（内部 es/retryDelay/reconnectTimer 不跨用例泄漏）。 */
async function loadStreamModule() {
  vi.resetModules();
  const mod = await import('./wikiStream');
  return mod;
}

const onWiki = vi.fn<(e: WikiEvent) => void>();
const onSync = vi.fn<(lagged: number) => void>();

beforeEach(() => {
  MockEventSource.instances = [];
  vi.stubGlobal('EventSource', MockEventSource);
  onWiki.mockReset();
  onSync.mockReset();
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
  localStorage.clear();
  vi.restoreAllMocks();
});

describe('wikiStream', () => {
  it('connects once with token URL and delivers wiki events', async () => {
    localStorage.setItem('auth_token', 'tok-1');
    const { wikiStream } = await loadStreamModule();

    const unsub = wikiStream.subscribe({ onWiki, onSync });

    expect(MockEventSource.instances).toHaveLength(1);
    expect(MockEventSource.instances[0].url).toBe(
      '/api/knowledge/events?token=tok-1',
    );

    // 统一 SSE 发 KbEvent（含 kind），stream 过滤 pages 映射回旧 WikiEvent 形状
    const ev = {
      doc_id: 'd1',
      kb_id: 'w1',
      kind: 'pages' as const,
      status: 'processing',
      chunk_count: 0,
      error: null,
    };
    MockEventSource.instances[0].dispatch('knowledge', JSON.stringify(ev));
    expect(onWiki).toHaveBeenCalledWith({
      wiki_id: 'w1',
      doc_id: 'd1',
      status: 'processing',
      page_count: 0,
      error: null,
    });

    // sync(lagged) 通知回调
    MockEventSource.instances[0].dispatch('sync', JSON.stringify({ lagged: 4 }));
    expect(onSync).toHaveBeenCalledWith(4);

    // 解析失败的事件被忽略，不崩溃
    MockEventSource.instances[0].dispatch('knowledge', '{bad json');
    expect(onWiki).toHaveBeenCalledTimes(1);

    unsub();
  });

  it('closes the EventSource when the last subscriber unsubscribes', async () => {
    const { wikiStream } = await loadStreamModule();
    const unsub = wikiStream.subscribe({ onWiki, onSync });
    expect(MockEventSource.instances).toHaveLength(1);
    unsub();
    expect(MockEventSource.instances[0].readyState).toBe(2); // CLOSED
  });

  it('reconnects with exponential backoff on error and resets on onopen', async () => {
    vi.useFakeTimers();
    const { wikiStream } = await loadStreamModule();
    wikiStream.subscribe({ onWiki, onSync });
    expect(MockEventSource.instances).toHaveLength(1);

    // 连接建立成功 → 重置退避到初始值
    MockEventSource.instances[0].onopen?.();
    // 出错 → 关闭并排 1s 重连
    MockEventSource.instances[0].onerror?.();
    expect(MockEventSource.instances[0].readyState).toBe(2);

    vi.advanceTimersByTime(1000);
    expect(MockEventSource.instances).toHaveLength(2);

    // 第二次出错 → 退避翻倍到 2s
    MockEventSource.instances[1].onerror?.();
    vi.advanceTimersByTime(1000);
    expect(MockEventSource.instances).toHaveLength(2); // 还没到 2s，未重连
    vi.advanceTimersByTime(1000);
    expect(MockEventSource.instances).toHaveLength(3);
  });
});
