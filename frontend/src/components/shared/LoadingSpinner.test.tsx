// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { LoadingSpinner } from './LoadingSpinner';

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe('LoadingSpinner', () => {
  it('renders default loading text', () => {
    const { container } = render(<LoadingSpinner />);
    expect(screen.getByText('Loading...')).toBeTruthy();
    const icon = container.querySelector('svg');
    expect(icon?.getAttribute('class')).toContain('animate-spin');
  });

  it('renders custom text', () => {
    render(<LoadingSpinner text="Fetching logs..." />);
    expect(screen.getByText('Fetching logs...')).toBeTruthy();
  });

  it('merges custom className', () => {
    const { container } = render(<LoadingSpinner className="custom-cls" />);
    expect(container.firstElementChild?.className).toContain('custom-cls');
  });
});
