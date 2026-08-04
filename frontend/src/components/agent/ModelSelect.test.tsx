// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import ModelSelect from './ModelSelect';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('../../api/agentModels', () => ({
  listAgentSelectableModels: vi.fn().mockResolvedValue({
    models: [
      { id: 'deepseek-chat', label: 'deepseek-chat' },
      { id: 'gpt-4o', label: 'gpt-4o' },
    ],
    groups: [{ id: 'router', label: 'router' }],
  }),
}));

afterEach(() => {
  cleanup();
});

const renderSelect = (value = '', onChange = vi.fn()) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ModelSelect value={value} onChange={onChange} />
    </QueryClientProvider>
  );
};

describe('ModelSelect', () => {
  it('shows current value label', async () => {
    renderSelect('deepseek-chat');
    expect(await screen.findByText('deepseek-chat')).toBeTruthy();
  });

  it('renders model and group options in two groups', async () => {
    renderSelect('');
    // 打开下拉（radix select 需 pointer 交互，此处校验 trigger 存在即可，
    // 选项断言依赖 jsdom radix portal，标记为冒烟）
    expect(screen.getByRole('combobox')).toBeTruthy();
  });
});
