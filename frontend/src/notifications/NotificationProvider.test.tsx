// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, render } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import type { AgentNotification } from '../types';
import {
  AgentNotificationsProvider,
  useAgentNotifications,
  type AgentNotificationsContextValue,
} from './NotificationProvider';

// 断言用翻译 key 而非具体文案（避免依赖 I18nProvider / 语言偏好）
vi.mock('react-i18next', () => ({
  useTranslation: () => ({
    t: (k: string) => k,
  }),
}));

// ── WebSocket 替身：捕获实例供手动触发 onmessage ───────────────
const wsInstances: FakeWs[] = [];
class FakeWs {
  static OPEN = 1;
  readyState = 1;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onopen: (() => void) | null = null;
  close = vi.fn(() => {
    this.onclose?.();
  });
  constructor(public url: string) {
    wsInstances.push(this);
  }
}

// ── Notification 替身：记录构造与权限请求 ──────────────────────
class MockNotification {
  static permission: NotificationPermission = 'granted';
  static requestPermission = vi.fn(
    () => Promise.resolve('granted' as NotificationPermission),
  );
  static instances: MockNotification[] = [];
  onclick: ((ev: Event) => void) | null = null;
  constructor(public title: string, public options: NotificationOptions) {
    MockNotification.instances.push(this);
  }
}

let hiddenValue = false;
function setTabVisibility(hidden: boolean) {
  hiddenValue = hidden;
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hiddenValue });
  Object.defineProperty(document, 'visibilityState', {
    configurable: true,
    get: () => (hiddenValue ? 'hidden' : 'visible'),
  });
}

// Probe：在 Provider 内拿到 context API，供测试调用 setActiveSessionId/setEnabled
let api: AgentNotificationsContextValue;
function Probe() {
  api = useAgentNotifications();
  return null;
}

const done: AgentNotification = { event: 'turn_done', session_id: 's1', workspace_id: 'w1' };

const renderProvider = () =>
  render(
    <MemoryRouter>
      <AgentNotificationsProvider>
        <Probe />
      </AgentNotificationsProvider>
    </MemoryRouter>,
  );

const fire = (ws: FakeWs, n: AgentNotification) =>
  act(() => {
    ws.onmessage?.({ data: JSON.stringify(n) });
  });

describe('AgentNotificationsProvider', () => {
  beforeEach(() => {
    localStorage.clear();
    hiddenValue = false;
    wsInstances.length = 0;
    MockNotification.instances = [];
    MockNotification.permission = 'granted';
    vi.clearAllMocks();
    document.title = 'Aurora Tunnel Admin';
    setTabVisibility(false);
    vi.stubGlobal('WebSocket', FakeWs as unknown as typeof WebSocket);
    vi.stubGlobal('Notification', MockNotification as unknown as typeof Notification);
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it('establishes one notification WS when enabled', () => {
    renderProvider();
    expect(wsInstances).toHaveLength(1);
    expect(wsInstances[0].url).toContain('/api/agent/notifications/ws');
  });

  it('background tab: notifies (flash title + system notification) for any session', () => {
    setTabVisibility(true); // 标签页在后台
    renderProvider();
    fire(wsInstances[0], done);
    expect(document.title).toContain('agent.notifTurnDone');
    expect(MockNotification.instances).toHaveLength(1);
    expect(MockNotification.instances[0].title).toContain('agent.notifTurnDone');
    // 通知点击：定位到对应会话并跳转 /agent
    expect(MockNotification.instances[0].onclick).toBeTypeOf('function');
  });

  it('visible tab on the watched session: skips', () => {
    setTabVisibility(false); // 前台
    renderProvider();
    act(() => api.setActiveSessionId('s1'));
    fire(wsInstances[0], done);
    expect(document.title).toBe('Aurora Tunnel Admin'); // 未闪烁
    expect(MockNotification.instances).toHaveLength(0);
  });

  it('visible tab on a different session: still notifies (workspace-wide)', () => {
    setTabVisibility(false);
    renderProvider();
    act(() => api.setActiveSessionId('other'));
    fire(wsInstances[0], done);
    expect(MockNotification.instances).toHaveLength(1);
  });

  it('restores the title when the tab becomes visible again', () => {
    setTabVisibility(true);
    renderProvider();
    fire(wsInstances[0], done);
    expect(document.title).toContain('agent.notifTurnDone');
    setTabVisibility(false);
    act(() => {
      document.dispatchEvent(new Event('visibilitychange'));
    });
    expect(document.title).toBe('Aurora Tunnel Admin');
  });

  it('toggling on requests browser notification permission', () => {
    renderProvider();
    act(() => api.setEnabled(true));
    expect(MockNotification.requestPermission).toHaveBeenCalled();
  });

  it('ignores heartbeat frames (no notification, no title flash)', () => {
    renderProvider();
    fire(wsInstances[0], { type: 'heartbeat', ts: 1720000000 } as unknown as AgentNotification);
    // 心跳仅探活：不触发标题闪烁、不弹系统通知
    expect(document.title).toBe('Aurora Tunnel Admin');
    expect(MockNotification.instances).toHaveLength(0);
  });

  it('watchdog closes a half-open connection with no frames for >75s', () => {
    // 捕获 30s 看门狗 interval 回调（与 ChatStream 测试同手法）；控制 Date.now：
    // onopen 建立基线后推进 >75s 模拟静默假死（半开 TCP 无 onclose）。
    let watchdogCb: (() => void) | undefined;
    const origSetInterval = globalThis.setInterval;
    const setIntervalSpy = vi.spyOn(globalThis, 'setInterval').mockImplementation(
      ((cb: () => void, ms?: number) => {
        if (ms === 30_000) watchdogCb = cb;
        return origSetInterval(cb, ms ?? 0) as ReturnType<typeof setInterval>;
      }) as typeof setInterval,
    );
    const nowSpy = vi.spyOn(Date, 'now').mockReturnValue(1_000_000);
    renderProvider();
    const ws = wsInstances[0];
    act(() => {
      ws.onopen?.();
    });
    expect(watchdogCb).toBeTypeOf('function');
    nowSpy.mockReturnValue(1_000_000 + 80_000);
    act(() => {
      watchdogCb?.();
    });
    // 看门狗判定假死 → 主动 close（close 触发 onclose → 指数退避重连 scheduled）
    expect(ws.close).toHaveBeenCalled();
    // 恢复本测试创建的 spy（文件 afterEach 只 unstub globals，不 restore spies）
    nowSpy.mockRestore();
    setIntervalSpy.mockRestore();
  });
});
