// @vitest-environment jsdom
import { cleanup, render } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Sparkline } from './Sparkline';

class ResizeObserverMock {
  observe() {}
  unobserve() {}
  disconnect() {}
}

beforeEach(() => {
  vi.stubGlobal('ResizeObserver', ResizeObserverMock);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe('Sparkline', () => {
  it('renders nothing when values are empty', () => {
    const { container } = render(<Sparkline values={[]} />);
    expect(container.firstChild).toBeNull();
  });

  it('renders chart container when values are present', () => {
    const { container } = render(<Sparkline values={[80, 90, 70, 95]} />);
    expect(container.querySelector('.recharts-responsive-container')).not.toBeNull();
  });
});
