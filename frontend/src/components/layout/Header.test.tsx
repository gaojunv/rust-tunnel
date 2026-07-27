// @vitest-environment jsdom
import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { beforeEach, describe, expect, it } from 'vitest';
import i18n from '@/i18n';
import { I18nProvider } from '@/i18n/I18nProvider';
import { ThemeProvider } from '@/theme/ThemeProvider';
import { Header } from './Header';

const renderHeader = () =>
  render(
    <MemoryRouter>
      <ThemeProvider>
        <I18nProvider>
          <Header onLogout={() => {}} />
        </I18nProvider>
      </ThemeProvider>
    </MemoryRouter>,
  );

describe('Header i18n', () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it('renders navigation in English', async () => {
    await i18n.changeLanguage('en');
    renderHeader();

    // Dashboard is always visible as a direct nav link
    expect(screen.getAllByText('Dashboard').length).toBeGreaterThan(0);
    // Group labels are visible on DropdownMenu triggers
    expect(screen.getAllByText('Network').length).toBeGreaterThan(0);
    expect(screen.getAllByText('System').length).toBeGreaterThan(0);
  });

  it('renders navigation in Chinese when preference is zh-CN', async () => {
    localStorage.setItem('rust-tunnel-language', 'zh-CN');
    renderHeader();

    expect(screen.getAllByText('仪表盘').length).toBeGreaterThan(0);
    expect(screen.getAllByText('网络').length).toBeGreaterThan(0);
    expect(screen.getAllByText('系统').length).toBeGreaterThan(0);
  });
});
