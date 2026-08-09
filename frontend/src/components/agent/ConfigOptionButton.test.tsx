// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import ConfigOptionButton from './ConfigOptionButton';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

afterEach(cleanup);

const modeOption = {
  id: 'mode', name: 'Mode', category: 'mode', type: 'select' as const,
  currentValue: 'plan',
  options: [
    { value: 'default', name: 'Default' },
    { value: 'plan', name: 'Plan' },
  ],
};

describe('ConfigOptionButton', () => {
  it('renders current option name on the trigger', () => {
    render(<ConfigOptionButton option={modeOption} label="agent.configMode" onChange={vi.fn()} />);
    expect(screen.getByText('Plan')).toBeTruthy();
  });

  it('renders nothing when option is undefined (non-ACP session)', () => {
    const { container } = render(
      <ConfigOptionButton option={undefined} label="agent.configMode" onChange={vi.fn()} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it('renders disabled placeholder when placeholder prop set and option is undefined', () => {
    render(
      <ConfigOptionButton
        option={undefined}
        label="agent.configEffort"
        onChange={vi.fn()}
        placeholder
      />,
    );
    const btn = screen.getByRole('button', { name: 'agent.configEffort' });
    expect(btn).toBeTruthy();
    expect((btn as HTMLButtonElement).disabled).toBe(true);
    expect(btn.getAttribute('title')).toBe('agent.configOptionUnsupported');
  });
});
