// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

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

  dispatch(type: string, data: string): void {
    const listeners = this.listeners.get(type);
    if (!listeners) return;
    listeners.forEach((cb) => cb({ data } as MessageEvent));
  }

  close(): void {
    this.readyState = 2;
  }
}

async function loadFactory() {
  vi.resetModules();
  const mod = await import('./sseStream');
  return mod;
}

beforeEach(() => {
  MockEventSource.instances = [];
  vi.stubGlobal('EventSource', MockEventSource);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
  localStorage.clear();
  vi.restoreAllMocks();
});

describe('createSseStream', () => {
  it('dispatches parsed payloads to subscribers and respects unsubscribe', async () => {
    localStorage.setItem('auth_token', 'tok-x');
    const { createSseStream } = await loadFactory();
    const stream = createSseStream<{ foo: { v: number }; bar: number }>({
      url: '/api/test/events',
      parsers: {
        foo: (raw) => JSON.parse(raw) as { v: number },
        bar: (raw) => (JSON.parse(raw) as { n: number }).n,
      },
    });

    const onFoo = vi.fn();
    const onBar = vi.fn();
    const unsub = stream.subscribe({ foo: onFoo, bar: onBar });

    expect(MockEventSource.instances).toHaveLength(1);
    expect(MockEventSource.instances[0].url).toBe('/api/test/events?token=tok-x');

    MockEventSource.instances[0].dispatch('foo', JSON.stringify({ v: 42 }));
    expect(onFoo).toHaveBeenCalledWith({ v: 42 });
    MockEventSource.instances[0].dispatch('bar', JSON.stringify({ n: 7 }));
    expect(onBar).toHaveBeenCalledWith(7);

    // bad JSON is swallowed
    MockEventSource.instances[0].dispatch('foo', '{bad');
    expect(onFoo).toHaveBeenCalledTimes(1);

    unsub();
    expect(MockEventSource.instances[0].readyState).toBe(2);
  });

  it('closes when last subscriber leaves, keeps open with one remaining', async () => {
    const { createSseStream } = await loadFactory();
    const stream = createSseStream<{ foo: number }>({
      url: '/api/test/events',
      parsers: { foo: (raw) => JSON.parse(raw) as number },
    });
    const unsub1 = stream.subscribe({ foo: vi.fn() });
    const unsub2 = stream.subscribe({ foo: vi.fn() });
    expect(MockEventSource.instances).toHaveLength(1);
    unsub1();
    expect(MockEventSource.instances[0].readyState).toBe(0);
    unsub2();
    expect(MockEventSource.instances[0].readyState).toBe(2);
  });

  it('reconnects with exponential backoff and resets on onopen', async () => {
    vi.useFakeTimers();
    const { createSseStream } = await loadFactory();
    const stream = createSseStream<{ foo: number }>({
      url: '/api/test/events',
      parsers: { foo: (raw) => JSON.parse(raw) as number },
    });
    stream.subscribe({ foo: vi.fn() });
    expect(MockEventSource.instances).toHaveLength(1);

    MockEventSource.instances[0].onopen?.();
    MockEventSource.instances[0].onerror?.();
    expect(MockEventSource.instances[0].readyState).toBe(2);

    vi.advanceTimersByTime(1000);
    expect(MockEventSource.instances).toHaveLength(2);

    MockEventSource.instances[1].onerror?.();
    vi.advanceTimersByTime(1000);
    expect(MockEventSource.instances).toHaveLength(2);
    vi.advanceTimersByTime(1000);
    expect(MockEventSource.instances).toHaveLength(3);
  });
});
