// @vitest-environment jsdom
import { describe, expect, it, beforeEach, vi } from 'vitest';
import {
  clearDraft,
  closePath,
  isDirty,
  loadOpenFiles,
  onDraftsChanged,
  openOrActivate,
  readDraft,
  saveOpenFiles,
  writeDraft,
  MAX_OPEN_FILES,
  type FileTabsState,
} from './fileTabsStore';

beforeEach(() => {
  localStorage.clear();
});

describe('loadOpenFiles', () => {
  it('returns null when nothing is stored', () => {
    expect(loadOpenFiles('w1')).toBeNull();
  });

  it('parses a valid stored state', () => {
    localStorage.setItem('agent.files.w1', JSON.stringify({ open: ['a.rs', 'b.rs'], active: 'b.rs' }));
    expect(loadOpenFiles('w1')).toEqual({ open: ['a.rs', 'b.rs'], active: 'b.rs' });
  });

  it('tolerates corrupted / invalid JSON', () => {
    localStorage.setItem('agent.files.w1', '{not json');
    expect(loadOpenFiles('w1')).toBeNull();
    localStorage.setItem('agent.files.w1', '"just a string"');
    expect(loadOpenFiles('w1')).toBeNull();
    localStorage.setItem('agent.files.w1', 'null');
    expect(loadOpenFiles('w1')).toBeNull();
  });

  it('rejects states with malformed shape', () => {
    localStorage.setItem('agent.files.w1', JSON.stringify({ active: 'a.rs' }));
    expect(loadOpenFiles('w1')).toBeNull();
    localStorage.setItem('agent.files.w1', JSON.stringify({ open: 'x', active: 'a.rs' }));
    expect(loadOpenFiles('w1')).toBeNull();
    localStorage.setItem('agent.files.w1', JSON.stringify({ open: ['a.rs'] }));
    expect(loadOpenFiles('w1')).toBeNull();
    localStorage.setItem('agent.files.w1', JSON.stringify({ open: ['a.rs'], active: 5 }));
    expect(loadOpenFiles('w1')).toBeNull();
  });

  it('drops non-string paths and fixes active to open[0] when active is stale', () => {
    localStorage.setItem(
      'agent.files.w1',
      JSON.stringify({ open: ['a.rs', 7, 'b.rs', null], active: 'gone.rs' }),
    );
    expect(loadOpenFiles('w1')).toEqual({ open: ['a.rs', 'b.rs'], active: 'a.rs' });
  });

  it('fixes active to empty string when open is empty', () => {
    localStorage.setItem('agent.files.w1', JSON.stringify({ open: [], active: 'x.rs' }));
    expect(loadOpenFiles('w1')).toEqual({ open: [], active: '' });
  });
});

describe('saveOpenFiles roundtrip', () => {
  it('persists and reloads the same state', () => {
    const state: FileTabsState = { open: ['a.rs', 'b.rs'], active: 'a.rs' };
    saveOpenFiles('w1', state);
    expect(loadOpenFiles('w1')).toEqual(state);
    // 不同 workspace 的 key 互不干扰
    expect(loadOpenFiles('w2')).toBeNull();
  });
});

describe('openOrActivate', () => {
  it('activates an already-open path without duplicating it', () => {
    const state: FileTabsState = { open: ['a.rs', 'b.rs'], active: 'a.rs' };
    expect(openOrActivate(state, 'b.rs')).toEqual({ open: ['a.rs', 'b.rs'], active: 'b.rs' });
    expect(openOrActivate(state, 'a.rs')).toEqual({ open: ['a.rs', 'b.rs'], active: 'a.rs' });
  });

  it('appends and activates a new path', () => {
    expect(openOrActivate({ open: ['a.rs'], active: 'a.rs' }, 'b.rs')).toEqual({
      open: ['a.rs', 'b.rs'],
      active: 'b.rs',
    });
  });

  it('evicts the oldest path FIFO when over MAX_OPEN_FILES', () => {
    const open = Array.from({ length: MAX_OPEN_FILES }, (_, i) => `f${i}.rs`);
    const next = openOrActivate({ open, active: 'f0.rs' }, 'new.rs');
    expect(next.open).toEqual([...open.slice(1), 'new.rs']);
    expect(next.active).toBe('new.rs');
    expect(next.open).not.toContain('f0.rs');
  });
});

describe('closePath', () => {
  it('removes an inactive path and keeps active unchanged', () => {
    expect(closePath({ open: ['a.rs', 'b.rs', 'c.rs'], active: 'b.rs' }, 'c.rs')).toEqual({
      open: ['a.rs', 'b.rs'],
      active: 'b.rs',
    });
  });

  it('activates the right neighbor when closing the active path', () => {
    expect(closePath({ open: ['a.rs', 'b.rs', 'c.rs'], active: 'b.rs' }, 'b.rs')).toEqual({
      open: ['a.rs', 'c.rs'],
      active: 'c.rs',
    });
  });

  it('activates the left neighbor when closing the last (rightmost) active path', () => {
    expect(closePath({ open: ['a.rs', 'b.rs', 'c.rs'], active: 'c.rs' }, 'c.rs')).toEqual({
      open: ['a.rs', 'b.rs'],
      active: 'b.rs',
    });
  });

  it('empties active when the last path is closed', () => {
    expect(closePath({ open: ['a.rs'], active: 'a.rs' }, 'a.rs')).toEqual({ open: [], active: '' });
  });

  it('is a no-op when the path is not open', () => {
    const state: FileTabsState = { open: ['a.rs'], active: 'a.rs' };
    expect(closePath(state, 'zzz.rs')).toBe(state);
  });
});

describe('draft lifecycle', () => {
  it('readDraft returns null when nothing written', () => {
    expect(readDraft('w1', 'a.rs')).toBeNull();
    expect(isDirty('w1', 'a.rs')).toBe(false);
  });

  it('writeDraft stores content and marks dirty', () => {
    writeDraft('w1', 'a.rs', 'let x = 1;');
    expect(readDraft('w1', 'a.rs')).toEqual({ draft: 'let x = 1;', dirty: true });
    expect(isDirty('w1', 'a.rs')).toBe(true);
  });

  it('clearDraft removes the entry and clears dirty', () => {
    writeDraft('w1', 'a.rs', 'let x = 1;');
    clearDraft('w1', 'a.rs');
    expect(readDraft('w1', 'a.rs')).toBeNull();
    expect(isDirty('w1', 'a.rs')).toBe(false);
  });

  it('drafts are scoped per workspace and per path', () => {
    writeDraft('w1', 'a.rs', 'one');
    writeDraft('w2', 'a.rs', 'two');
    writeDraft('w1', 'b.rs', 'three');
    expect(readDraft('w1', 'a.rs')?.draft).toBe('one');
    expect(readDraft('w2', 'a.rs')?.draft).toBe('two');
    expect(readDraft('w1', 'b.rs')?.draft).toBe('three');
  });

  it('notifies subscribers on write and clear', () => {
    const listener = vi.fn();
    const unsubscribe = onDraftsChanged(listener);
    writeDraft('w1', 'a.rs', 'x');
    writeDraft('w1', 'a.rs', 'y');
    expect(listener).toHaveBeenCalledTimes(2);
    // 幂等写入同一内容仍会通知（调用方自行去重）
    clearDraft('w1', 'a.rs');
    expect(listener).toHaveBeenCalledTimes(3);
    unsubscribe();
    clearDraft('w1', 'a.rs');
    expect(listener).toHaveBeenCalledTimes(3);
  });
});