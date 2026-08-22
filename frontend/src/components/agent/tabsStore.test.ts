// @vitest-environment jsdom
import { describe, expect, it, beforeEach } from 'vitest';
import {
  loadTabs,
  saveTabs,
  migrateLegacy,
  reconcile,
  openOrActivate,
  closeTab,
  writePendingActivate,
  takePendingActivate,
  MAX_TABS,
  type TabState,
} from './tabsStore';

beforeEach(() => {
  localStorage.clear();
});

describe('loadTabs', () => {
  it('returns null when nothing is stored', () => {
    expect(loadTabs('w1')).toBeNull();
  });

  it('parses a valid stored state', () => {
    localStorage.setItem('agent.openTabs.w1', JSON.stringify({ open: ['a', 'b'], active: 'b' }));
    expect(loadTabs('w1')).toEqual({ open: ['a', 'b'], active: 'b' });
  });

  it('tolerates corrupted / invalid JSON', () => {
    localStorage.setItem('agent.openTabs.w1', '{not json');
    expect(loadTabs('w1')).toBeNull();
    localStorage.setItem('agent.openTabs.w1', '"just a string"');
    expect(loadTabs('w1')).toBeNull();
    localStorage.setItem('agent.openTabs.w1', 'null');
    expect(loadTabs('w1')).toBeNull();
  });

  it('rejects states with malformed shape', () => {
    // open 缺失 / 非数组
    localStorage.setItem('agent.openTabs.w1', JSON.stringify({ active: 'a' }));
    expect(loadTabs('w1')).toBeNull();
    localStorage.setItem('agent.openTabs.w1', JSON.stringify({ open: 'x', active: 'a' }));
    expect(loadTabs('w1')).toBeNull();
    // active 缺失 / 非字符串
    localStorage.setItem('agent.openTabs.w1', JSON.stringify({ open: ['a'] }));
    expect(loadTabs('w1')).toBeNull();
    localStorage.setItem('agent.openTabs.w1', JSON.stringify({ open: ['a'], active: 5 }));
    expect(loadTabs('w1')).toBeNull();
  });

  it('drops non-string ids and fixes active to open[0] when active is stale', () => {
    localStorage.setItem(
      'agent.openTabs.w1',
      JSON.stringify({ open: ['a', 7, 'b', null], active: 'gone' }),
    );
    expect(loadTabs('w1')).toEqual({ open: ['a', 'b'], active: 'a' });
  });

  it('fixes active to empty string when open is empty', () => {
    localStorage.setItem('agent.openTabs.w1', JSON.stringify({ open: [], active: 'x' }));
    expect(loadTabs('w1')).toEqual({ open: [], active: '' });
  });
});

describe('saveTabs roundtrip', () => {
  it('persists and reloads the same state', () => {
    const state: TabState = { open: ['s1', 's2'], active: 's1' };
    saveTabs('w1', state);
    expect(loadTabs('w1')).toEqual(state);
    // 不同 workspace 的 key 互不干扰
    expect(loadTabs('w2')).toBeNull();
  });
});

describe('migrateLegacy', () => {
  it('migrates when lastWorkspaceId matches and lastSessionId is set', () => {
    localStorage.setItem('agent.lastWorkspaceId', 'w1');
    localStorage.setItem('agent.lastSessionId', 's-old');
    expect(migrateLegacy('w1')).toEqual({ open: ['s-old'], active: 's-old' });
    // 迁移后旧 key 被删除
    expect(localStorage.getItem('agent.lastWorkspaceId')).toBeNull();
    expect(localStorage.getItem('agent.lastSessionId')).toBeNull();
  });

  it('does not migrate when workspace does not match', () => {
    localStorage.setItem('agent.lastWorkspaceId', 'w1');
    localStorage.setItem('agent.lastSessionId', 's-old');
    expect(migrateLegacy('w2')).toBeNull();
    // 旧 key 保留
    expect(localStorage.getItem('agent.lastWorkspaceId')).toBe('w1');
    expect(localStorage.getItem('agent.lastSessionId')).toBe('s-old');
  });

  it('does not migrate when lastSessionId is empty', () => {
    localStorage.setItem('agent.lastWorkspaceId', 'w1');
    localStorage.setItem('agent.lastSessionId', '');
    expect(migrateLegacy('w1')).toBeNull();
  });

  it('returns null when nothing stored', () => {
    expect(migrateLegacy('w1')).toBeNull();
  });
});

describe('reconcile', () => {
  it('filters out ids that no longer exist in the session list', () => {
    expect(
      reconcile({ open: ['a', 'b', 'c'], active: 'c' }, ['a', 'c', 'd']),
    ).toEqual({ open: ['a', 'c'], active: 'c' });
  });

  it('falls back active to the first remaining tab when active was removed', () => {
    expect(reconcile({ open: ['a', 'b', 'c'], active: 'b' }, ['a', 'c'])).toEqual({
      open: ['a', 'c'],
      active: 'a',
    });
  });

  it('empties active when nothing remains', () => {
    expect(reconcile({ open: ['a'], active: 'a' }, [])).toEqual({ open: [], active: '' });
  });
});

describe('openOrActivate', () => {
  it('activates an already-open tab without duplicating it', () => {
    const state: TabState = { open: ['a', 'b'], active: 'a' };
    expect(openOrActivate(state, 'b')).toEqual({ open: ['a', 'b'], active: 'b' });
    expect(openOrActivate(state, 'a')).toEqual({ open: ['a', 'b'], active: 'a' });
  });

  it('appends and activates a new tab', () => {
    expect(openOrActivate({ open: ['a'], active: 'a' }, 'b')).toEqual({
      open: ['a', 'b'],
      active: 'b',
    });
  });

  it('evicts the oldest tab FIFO when over MAX_TABS', () => {
    const open = Array.from({ length: MAX_TABS }, (_, i) => `s${i}`);
    const next = openOrActivate({ open, active: 's0' }, 'new');
    expect(next.open).toEqual([...open.slice(1), 'new']);
    expect(next.active).toBe('new');
    // 已被淘汰的 id 不在结果中
    expect(next.open).not.toContain('s0');
  });
});

describe('closeTab', () => {
  it('removes an inactive tab and keeps active unchanged', () => {
    expect(closeTab({ open: ['a', 'b', 'c'], active: 'b' }, 'c')).toEqual({
      open: ['a', 'b'],
      active: 'b',
    });
  });

  it('activates the right neighbor when closing the active tab', () => {
    expect(closeTab({ open: ['a', 'b', 'c'], active: 'b' }, 'b')).toEqual({
      open: ['a', 'c'],
      active: 'c',
    });
  });

  it('activates the left neighbor when closing the last (rightmost) active tab', () => {
    expect(closeTab({ open: ['a', 'b', 'c'], active: 'c' }, 'c')).toEqual({
      open: ['a', 'b'],
      active: 'b',
    });
  });

  it('empties active when the last tab is closed', () => {
    expect(closeTab({ open: ['a'], active: 'a' }, 'a')).toEqual({ open: [], active: '' });
  });

  it('is a no-op when the id is not open', () => {
    const state: TabState = { open: ['a'], active: 'a' };
    expect(closeTab(state, 'zzz')).toBe(state);
  });
});

describe('pendingActivate', () => {
  it('round-trips and is consumed once', () => {
    writePendingActivate('w1', 's1');
    expect(takePendingActivate()).toEqual({ workspaceId: 'w1', sessionId: 's1' });
    // 一次性语义：第二次取为空
    expect(takePendingActivate()).toBeNull();
  });

  it('returns null for missing / corrupted / malformed entries', () => {
    expect(takePendingActivate()).toBeNull();
    localStorage.setItem('agent.pendingActivateSession', '{bad json');
    expect(takePendingActivate()).toBeNull();
    localStorage.setItem('agent.pendingActivateSession', JSON.stringify({ workspaceId: 'w1' }));
    expect(takePendingActivate()).toBeNull();
    localStorage.setItem('agent.pendingActivateSession', JSON.stringify({ workspaceId: '', sessionId: 's1' }));
    expect(takePendingActivate()).toBeNull();
  });
});
