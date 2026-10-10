// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { ErrorState, resolveErrorMessage } from './ErrorState';

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe('ErrorState', () => {
  it('message prop overrides error resolution', () => {
    render(<ErrorState error={{ response: { data: { error: 'boom' } } }} message="Custom failure" />);
    expect(screen.getByText('Custom failure')).toBeTruthy();
    expect(screen.queryByText('boom')).toBeNull();
  });

  it('resolves axios-like error via getApiErrorMessage', () => {
    render(<ErrorState error={{ response: { data: { error: 'boom' } } }} />);
    expect(screen.getByText('boom')).toBeTruthy();
  });

  it('falls back to common.loadFailed for plain objects', () => {
    // 纯对象会被 getApiErrorMessage 还原为 "[object Object]"，无展示意义，应回退默认文案
    render(<ErrorState error={{}} />);
    expect(screen.getByText('Failed to load data')).toBeTruthy();
  });

  it('renders retry button when onRetry provided and fires on click', () => {
    const onRetry = vi.fn();
    render(<ErrorState message="oops" onRetry={onRetry} />);
    const btn = screen.getByText('Retry');
    fireEvent.click(btn);
    expect(onRetry).toHaveBeenCalledTimes(1);
  });

  it('hides retry button when no onRetry', () => {
    render(<ErrorState message="oops" />);
    expect(screen.queryByText('Retry')).toBeNull();
  });
});

describe('resolveErrorMessage', () => {
  it('returns fallback for nullish errors', () => {
    expect(resolveErrorMessage(undefined, 'FB')).toBe('FB');
    expect(resolveErrorMessage(null, 'FB')).toBe('FB');
  });

  it('resolves string body errors', () => {
    expect(resolveErrorMessage({ response: { data: 'bad name' } }, 'FB')).toBe('bad name');
  });

  it('returns fallback for unresolvable objects', () => {
    expect(resolveErrorMessage({}, 'FB')).toBe('FB');
  });
});
