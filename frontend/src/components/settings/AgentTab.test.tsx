// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import AgentTab from './AgentTab';

// 通知上下文替身：仅断言 UI 到 setEnabled 的接线（权限请求行为在
// NotificationProvider.test 单独覆盖）。
const notificationsApi = vi.hoisted(() => ({
  enabled: true,
  permission: 'default' as 'default' | 'granted' | 'denied',
  setEnabled: vi.fn(),
  setActiveSessionId: vi.fn(),
}));

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('@/api/client', () => ({
  getAgentDefaultModel: () => Promise.resolve(''),
  putAgentDefaultModel: () => Promise.resolve(),
  getApiErrorMessage: (e: unknown) => String(e),
}));

vi.mock('@/api/agentModels', () => ({
  listAgentSelectableModels: () => Promise.resolve({ models: [], groups: [] }),
}));

vi.mock('@/notifications/NotificationProvider', () => ({
  useAgentNotifications: () => notificationsApi,
}));

const renderTab = () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <AgentTab />
    </QueryClientProvider>,
  );
};

describe('AgentTab browser-notification toggle', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  afterEach(() => {
    cleanup();
  });

  it('renders the toggle and reflects the enabled state', async () => {
    renderTab();
    const sw = await screen.findByRole('switch');
    expect(sw.getAttribute('data-state')).toBe('checked');
  });

  it('calls setEnabled(false) when toggled off', async () => {
    renderTab();
    const sw = await screen.findByRole('switch');
    fireEvent.click(sw);
    expect(notificationsApi.setEnabled).toHaveBeenCalledWith(false);
  });

  it('shows the blocked hint when permission is denied', async () => {
    notificationsApi.permission = 'denied';
    renderTab();
    expect(screen.getByText('settings.agent.notificationsPermissionDenied')).toBeTruthy();
  });
});
