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
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
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
});
