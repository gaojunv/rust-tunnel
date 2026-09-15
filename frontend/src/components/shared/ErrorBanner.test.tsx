// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { ErrorBanner } from './ErrorBanner';

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe('ErrorBanner', () => {
  it('renders resolved message with banner styling', () => {
    const { container } = render(<ErrorBanner error={{ response: { data: { error: 'boom' } } }} />);
    expect(screen.getByText('boom')).toBeTruthy();
    const banner = screen.getByRole('alert');
    expect(banner.className).toContain('border-destructive/30');
    expect(banner.className).toContain('bg-destructive/10');
    expect(container.querySelector('button')).toBeNull();
  });

  it('message prop overrides error resolution', () => {
    render(<ErrorBanner message="Section failed" />);
    expect(screen.getByText('Section failed')).toBeTruthy();
  });

  it('falls back to common.loadFailed for plain objects', () => {
    render(<ErrorBanner error={{}} />);
    expect(screen.getByText('Failed to load data')).toBeTruthy();
  });

  it('renders retry icon-button when onRetry provided and fires on click', () => {
    const onRetry = vi.fn();
    const { container } = render(<ErrorBanner message="oops" onRetry={onRetry} />);
    const btn = screen.getByRole('button', { name: 'Retry' });
    expect(container.querySelector('svg')?.getAttribute('class')).toContain('h-3.5');
    fireEvent.click(btn);
    expect(onRetry).toHaveBeenCalledTimes(1);
  });

  it('hides retry button when no onRetry', () => {
    const { container } = render(<ErrorBanner message="oops" />);
    expect(container.querySelector('button')).toBeNull();
  });
});
