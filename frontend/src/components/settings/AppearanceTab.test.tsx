// @vitest-environment jsdom
import { render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';
import i18n from '@/i18n';
import { I18nProvider } from '@/i18n/I18nProvider';
import { LANGUAGE_STORAGE_KEY } from '@/i18n/languagePreference';
import AppearanceTab from './AppearanceTab';

const renderTab = () =>
  render(
    <I18nProvider>
      <AppearanceTab />
    </I18nProvider>,
  );

describe('AppearanceTab', () => {
  beforeEach(async () => {
    localStorage.clear();
    await i18n.changeLanguage('en');
  });

  it('shows the language section', async () => {
    await i18n.changeLanguage('en');
    renderTab();

    expect(screen.getByText('Appearance')).toBeTruthy();
    expect(screen.getByText('Language')).toBeTruthy();
  });

  it('renders Chinese labels when preference is zh-CN', async () => {
    localStorage.setItem(LANGUAGE_STORAGE_KEY, 'zh-CN');
    renderTab();

    // Wait for I18nProvider to apply resolvedLanguage
    const titles = await screen.findAllByText('外观');
    expect(titles.length).toBeGreaterThanOrEqual(1);
    const labels = await screen.findAllByText('语言');
    expect(labels.length).toBeGreaterThanOrEqual(1);
  });
});
