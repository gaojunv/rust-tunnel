// @vitest-environment jsdom
import { beforeEach, describe, expect, it } from 'vitest';
import type { AgentNotification } from '../types';
import {
  getNotificationsEnabled,
  setNotificationsEnabled,
  shouldNotify,
} from './agentNotificationSettings';

const done: AgentNotification = { event: 'turn_done', session_id: 's1', workspace_id: 'w1' };

describe('getNotificationsEnabled / setNotificationsEnabled', () => {
  beforeEach(() => localStorage.clear());

  it('defaults to enabled when unset or storage unavailable', () => {
    expect(getNotificationsEnabled()).toBe(true);
    expect(getNotificationsEnabled(undefined)).toBe(true);
  });

  it('persists the toggle', () => {
    setNotificationsEnabled(false);
    expect(getNotificationsEnabled()).toBe(false);
    setNotificationsEnabled(true);
    expect(getNotificationsEnabled()).toBe(true);
  });

  it('accepts an explicit storage and tolerates corrupt values', () => {
    const store = new Map<string, string>();
    const storage = {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => void store.set(k, v),
      removeItem: (k: string) => void store.delete(k),
    };
    setNotificationsEnabled(false, storage);
    expect(getNotificationsEnabled(storage)).toBe(false);
    store.set('agent.notificationsEnabled', 'garbage');
    expect(getNotificationsEnabled(storage)).toBe(true); // 损坏回退默认
  });
});

describe('shouldNotify', () => {
  it('never notifies when disabled', () => {
    expect(shouldNotify(done, { enabled: false, activeSessionId: null, tabVisible: false })).toBe(
      false,
    );
  });

  it('skips the session the user is actively watching in a visible tab', () => {
    expect(shouldNotify(done, { enabled: true, activeSessionId: 's1', tabVisible: true })).toBe(
      false,
    );
  });

  it('notifies a different session even in a visible tab (workspace-wide)', () => {
    expect(
      shouldNotify(done, { enabled: true, activeSessionId: 'other-session', tabVisible: true }),
    ).toBe(true);
  });

  it('notifies the watched session when the tab is in the background', () => {
    expect(shouldNotify(done, { enabled: true, activeSessionId: 's1', tabVisible: false })).toBe(
      true,
    );
  });

  it('notifies any session when no session is open', () => {
    expect(shouldNotify(done, { enabled: true, activeSessionId: null, tabVisible: true })).toBe(
      true,
    );
  });
});
