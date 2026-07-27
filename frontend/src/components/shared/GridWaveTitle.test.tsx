// @vitest-environment jsdom
import { render } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { GridWaveTitle } from './GridWaveTitle';

describe('GridWaveTitle', () => {
  beforeEach(() => {
    // 默认非 reduced-motion
    window.matchMedia = vi.fn().mockImplementation((query: string) => ({
      matches: false,
      media: query,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      addListener: vi.fn(),
      removeListener: vi.fn(),
      onchange: null,
      dispatchEvent: vi.fn(),
    }));
  });

  it('renders the text in a span (not canvas-only)', () => {
    const { container } = render(<GridWaveTitle text="Hello" />);
    expect(container.textContent).toContain('Hello');
  });

  it('renders a canvas behind the text when motion allowed', () => {
    const { container } = render(<GridWaveTitle text="Hello" />);
    const canvas = container.querySelector('canvas');
    expect(canvas).not.toBeNull();
  });

  it('degrades to plain span when prefers-reduced-motion', () => {
    window.matchMedia = vi.fn().mockImplementation((query: string) => ({
      matches: query === '(prefers-reduced-motion: reduce)',
      media: query,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      addListener: vi.fn(),
      removeListener: vi.fn(),
      onchange: null,
      dispatchEvent: vi.fn(),
    }));
    const { container } = render(<GridWaveTitle text="Hello" />);
    expect(container.querySelector('canvas')).toBeNull();
    expect(container.textContent).toContain('Hello');
  });

  it('has accessible text via span (sr-only or visible)', () => {
    const { container } = render(<GridWaveTitle text="Dashboard" />);
    // 文字必须出现在 DOM 中（SEO + 屏幕阅读器）
    expect(container.textContent).toContain('Dashboard');
  });
});
