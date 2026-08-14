import { describe, expect, it } from 'vitest';
import {
  conclusionBadgeClass,
  conclusionLabel,
  formatGhTime,
  githubErrorKind,
  githubErrorTitleKey,
  isRunActive,
  runStatusLabel,
  serializeGhInputs,
} from './githubUtils';

/**
 * 模拟 i18next 的 key 解析：已知 key 有译文，未知 key（未在语言包里注册）返回原 key。
 * 与真实行为一致——runStatusLabel/conclusionLabel 正是靠「t(key) !== key」判断已知状态。
 */
const translations: Record<string, string> = {
  'agent.githubRunStatus_queued': '排队中',
  'agent.githubRunStatus_in_progress': '进行中',
  'agent.githubRunConclusion_success': '成功',
  'agent.githubRunConclusion_timed_out': '超时',
  'agent.timeMinutesAgo': '5 分钟前',
};
const t = (k: string) => translations[k] ?? k;

describe('runStatusLabel / conclusionLabel', () => {
  it('translates known statuses via i18n key; falls back to raw status for unknown', () => {
    expect(runStatusLabel('queued', t)).toBe('排队中');
    expect(runStatusLabel('in_progress', t)).toBe('进行中');
    // 未知状态（新取值）回退原文
    expect(runStatusLabel('superseded', t)).toBe('superseded');
    expect(runStatusLabel(null, t)).toBe('agent.githubStatusUnknown');
  });

  it('translates known conclusions; null/undefined → empty string; unknown → raw', () => {
    expect(conclusionLabel('success', t)).toBe('成功');
    expect(conclusionLabel('timed_out', t)).toBe('超时');
    expect(conclusionLabel(null, t)).toBe('');
    expect(conclusionLabel(undefined, t)).toBe('');
    expect(conclusionLabel('weird_conclusion', t)).toBe('weird_conclusion');
  });
});

describe('isRunActive', () => {
  it('treats queue/waiting/in-progress as active, completed as inactive', () => {
    expect(isRunActive('queued')).toBe(true);
    expect(isRunActive('in_progress')).toBe(true);
    expect(isRunActive('waiting')).toBe(true);
    expect(isRunActive('requested')).toBe(true);
    expect(isRunActive('pending')).toBe(true);
    expect(isRunActive('completed')).toBe(false);
    expect(isRunActive('')).toBe(false);
    expect(isRunActive(null)).toBe(false);
  });
});

describe('conclusionBadgeClass', () => {
  it('success → green, failure/timed_out → red, rest → neutral', () => {
    expect(conclusionBadgeClass('success')).toContain('text-green-600');
    expect(conclusionBadgeClass('failure')).toContain('text-red-600');
    expect(conclusionBadgeClass('timed_out')).toContain('text-red-600');
    expect(conclusionBadgeClass('cancelled')).toContain('text-muted-foreground');
    expect(conclusionBadgeClass(null)).toContain('text-muted-foreground');
  });
});

describe('formatGhTime', () => {
  it('parses GitHub ISO time to relative text via formatRelativeTime', () => {
    // 相对真实当前时间（formatGhTime 内部用 Date.now()），5 分钟前稳定落在分钟档
    const iso = new Date(Date.now() - 5 * 60 * 1000).toISOString();
    expect(formatGhTime(iso, t)).toBe('5 分钟前');
  });

  it('returns empty string for invalid/missing iso', () => {
    expect(formatGhTime('', t)).toBe('');
    expect(formatGhTime(null, t)).toBe('');
    expect(formatGhTime('not-a-date', t)).toBe('');
  });
});

describe('githubErrorKind / githubErrorTitleKey', () => {
  it('classifies by HTTP status and maps to title keys', () => {
    expect(githubErrorKind({ response: { status: 429 } })).toBe('ratelimit');
    expect(githubErrorTitleKey('ratelimit')).toBe('agent.githubError_ratelimit');

    expect(githubErrorKind({ response: { status: 400 } })).toBe('invalid-token');
    expect(githubErrorTitleKey('invalid-token')).toBe('agent.githubError_invalid-token');

    expect(githubErrorKind({ response: { status: 502 } })).toBe('upstream');
    expect(githubErrorTitleKey('upstream')).toBe('agent.githubError_upstream');

    expect(githubErrorKind({ response: { status: 503 } })).toBe('other');
    expect(githubErrorKind(new Error('boom'))).toBe('other');
    expect(githubErrorTitleKey('other')).toBe('agent.githubError_other');
  });
});

describe('serializeGhInputs', () => {
  it('keeps non-empty keys, drops empty-key rows', () => {
    expect(
      serializeGhInputs([
        { key: 'env', value: 'prod' },
        { key: '', value: 'ignored' },
        { key: '  ', value: 'ignored' },
        { key: 'ref', value: 'main' },
      ]),
    ).toEqual({ env: 'prod', ref: 'main' });
    expect(serializeGhInputs([])).toEqual({});
  });
});
