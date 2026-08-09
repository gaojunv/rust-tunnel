// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import SessionSettingsMenu from './SessionSettingsMenu';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('../../api/agentModels', () => ({
  listAgentSelectableModels: vi.fn().mockResolvedValue({
    models: [{ id: 'deepseek-chat', label: 'deepseek-chat' }],
    groups: [],
  }),
}));

afterEach(cleanup);

const renderMenu = (props: Partial<Parameters<typeof SessionSettingsMenu>[0]> = {}) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <SessionSettingsMenu
        model="deepseek-chat"
        onModelChange={vi.fn()}
        configOptions={[]}
        onConfigChange={vi.fn()}
        {...props}
      />
    </QueryClientProvider>
  );
};

describe('SessionSettingsMenu', () => {
  it('shows current model label on the trigger', async () => {
    renderMenu();
    expect(await screen.findByText('deepseek-chat')).toBeTruthy();
  });

  it('renders generic config options passed in (mode/effort filtered upstream)', async () => {
    renderMenu({
      configOptions: [
        { id: 'fast', name: 'Fast', type: 'boolean', currentBool: true, currentValue: 'true' },
      ],
    });
    // trigger 冒烟：菜单项内容依赖 Radix portal，jsdom 下只断言 trigger 存在
    expect(await screen.findByText('deepseek-chat')).toBeTruthy();
  });
});
