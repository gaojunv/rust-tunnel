import { describe, expect, it } from 'vitest';
import i18n from './index';

describe('i18n instance', () => {
  it('translates zh-CN', async () => {
    await i18n.changeLanguage('zh-CN');
    expect(i18n.t('common.save')).toBe('保存');
  });

  it('translates en', async () => {
    await i18n.changeLanguage('en');
    expect(i18n.t('common.save')).toBe('Save');
  });

  it('falls back to en for a key missing in zh-CN', async () => {
    // 通过动态注入一个仅存在于 en 的 key 来验证 fallback 行为
    i18n.addResource('en', 'common', 'testOnly.fallbackProbe', 'probe-en');
    await i18n.changeLanguage('zh-CN');
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    expect((i18n as any).t('testOnly.fallbackProbe')).toBe('probe-en');
  });

  it('returns the key itself for a totally unknown key', async () => {
    await i18n.changeLanguage('en');
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    expect((i18n as any).t('no.such.key.exists')).toBe('no.such.key.exists');
  });
});
