// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';
import { branchNameFromHeader, formatCommitDate, parsePorcelainEntries } from './gitUtils';

const t = (k: string) => k;

describe('branchNameFromHeader', () => {
  it('takes the local part before "..."', () => {
    expect(branchNameFromHeader('main...origin/main')).toBe('main');
    expect(branchNameFromHeader('main...origin/main [ahead 1]')).toBe('main');
  });

  it('keeps plain branch names and null input', () => {
    expect(branchNameFromHeader('fix/abc')).toBe('fix/abc');
    expect(branchNameFromHeader(null)).toBeNull();
    expect(branchNameFromHeader('')).toBeNull();
  });
});

describe('formatCommitDate', () => {
  it('formats ISO date into relative buckets', () => {
    const now = Date.parse('2026-08-14T12:00:00Z');
    expect(formatCommitDate('2026-08-14T11:59:30Z', now, t)).toBe('agent.timeJustNow');
    expect(formatCommitDate('2026-08-14T11:58:00Z', now, t)).toBe('agent.timeMinutesAgo');
    expect(formatCommitDate('2026-08-14T09:00:00Z', now, t)).toBe('agent.timeHoursAgo');
    expect(formatCommitDate('2026-08-13T00:00:00Z', now, t)).toBe('agent.timeYesterday');
    expect(formatCommitDate('2026-08-01T00:00:00Z', now, t)).toBe('agent.timeDaysAgo');
  });

  it('returns empty string for invalid dates', () => {
    expect(formatCommitDate('', Date.now(), t)).toBe('');
    expect(formatCommitDate('not-a-date', Date.now(), t)).toBe('');
  });
});

describe('parsePorcelainEntries', () => {
  it('parses a typical mixed status output', () => {
    const entries = parsePorcelainEntries(`## main
M  src/lib.rs
 M src/main.rs
 D old.rs
R  a.txt -> b.txt
?? notes.md
`);
    expect(entries).toHaveLength(5);
    expect(entries.map((e) => e.path)).toEqual([
      'src/lib.rs',
      'src/main.rs',
      'old.rs',
      'b.txt',
      'notes.md',
    ]);
    expect(entries[0]).toMatchObject({ status: 'modified', staged: true });
    expect(entries[1]).toMatchObject({ status: 'modified', staged: false });
    expect(entries[2]).toMatchObject({ status: 'deleted', staged: false });
    expect(entries[3]).toMatchObject({ status: 'renamed', staged: true });
    expect(entries[4]).toMatchObject({ status: 'untracked', staged: false });
  });
});
