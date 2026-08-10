// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { DropdownMenu, DropdownMenuContent } from '@/components/ui/dropdown-menu';
import ModelPicker from './ModelPicker';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

afterEach(cleanup);

const models = [
  { id: 'deepseek-chat', label: 'deepseek-chat（DeepSeek）' },
  { id: 'claude-sonnet', label: 'claude-sonnet（Anthropic）' },
];
const groups = [{ id: 'fast-models', label: 'fast-models' }];

const renderPicker = (props: Partial<Parameters<typeof ModelPicker>[0]> = {}) =>
  render(
    <DropdownMenu open>
      <DropdownMenuContent align="start" className="w-56">
        <ModelPicker
          models={models}
          groups={groups}
          currentModel="deepseek-chat"
          onSelect={vi.fn()}
          {...props}
        />
      </DropdownMenuContent>
    </DropdownMenu>
  );

describe('ModelPicker', () => {
  it('renders search input, model/group sections and all items', () => {
    renderPicker();
    expect(screen.getByPlaceholderText('agent.searchModels')).toBeTruthy();
    expect(screen.getByText('agent.model')).toBeTruthy();
    expect(screen.getByText('agent.modelGroups')).toBeTruthy();
    expect(screen.getByText('deepseek-chat（DeepSeek）')).toBeTruthy();
    expect(screen.getByText('claude-sonnet（Anthropic）')).toBeTruthy();
    expect(screen.getByText('fast-models')).toBeTruthy();
  });

  it('filters models and groups by query (case-insensitive on id/label)', () => {
    renderPicker();
    fireEvent.change(screen.getByPlaceholderText('agent.searchModels'), {
      target: { value: 'DEEPSEEK' },
    });
    expect(screen.getByText('deepseek-chat（DeepSeek）')).toBeTruthy();
    expect(screen.queryByText('claude-sonnet（Anthropic）')).toBeNull();
    expect(screen.queryByText('fast-models')).toBeNull();
  });

  it('shows no-results message when nothing matches', () => {
    renderPicker();
    fireEvent.change(screen.getByPlaceholderText('agent.searchModels'), {
      target: { value: 'zzz-no-such' },
    });
    expect(screen.getByText('agent.noModelsFound')).toBeTruthy();
    expect(screen.queryByText('agent.model')).toBeNull();
  });

  it('invokes onSelect when a model is chosen', () => {
    const onSelect = vi.fn();
    renderPicker({ onSelect, currentModel: 'claude-sonnet' });
    fireEvent.click(screen.getByText('deepseek-chat（DeepSeek）'));
    expect(onSelect).toHaveBeenCalledWith('deepseek-chat');
  });
});
