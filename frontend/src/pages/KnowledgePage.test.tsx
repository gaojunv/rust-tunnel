import { describe, expect, it } from 'vitest';
import { resolveKnowTab } from './KnowledgePage';

describe('resolveKnowTab', () => {
  it('keeps valid tabs as-is without flagging an alias', () => {
    for (const tab of ['kb', 'memory', 'skill', 'roles'] as const) {
      expect(resolveKnowTab(tab)).toEqual({ tab, aliased: undefined });
    }
  });

  it('redirects the legacy ?tab=wiki deep link onto the unified kb tab', () => {
    expect(resolveKnowTab('wiki')).toEqual({ tab: 'kb', aliased: 'kb' });
  });

  it('falls back to kb for missing or unknown values, without flagging an alias', () => {
    expect(resolveKnowTab(null)).toEqual({ tab: 'kb', aliased: undefined });
    expect(resolveKnowTab('')).toEqual({ tab: 'kb', aliased: undefined });
    expect(resolveKnowTab('bogus')).toEqual({ tab: 'kb', aliased: undefined });
  });
});
