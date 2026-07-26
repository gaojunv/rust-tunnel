// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { ParticleTitle } from './ParticleTitle';

const mockMatchMedia = (reduced: boolean) => {
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    value: vi.fn().mockImplementation((query: string) => ({
      matches: reduced && query.includes('prefers-reduced-motion'),
      media: query,
      onchange: null,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      addListener: vi.fn(),
      removeListener: vi.fn(),
      dispatchEvent: vi.fn(),
    })),
  });
};

describe('ParticleTitle', () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    document.documentElement.style.setProperty('--primary', '221 83% 53%');
    mockMatchMedia(false);
  });

  afterEach(() => cleanup());

  it('renders a canvas with img role + aria-label and sr-only text when particles are available', () => {
    render(<ParticleTitle text="Clients" />);
    const canvas = screen.getByRole('img', { name: 'Clients' });
    expect(canvas.tagName).toBe('CANVAS');
    const srText = screen.getByText('Clients');
    expect(srText.className).toContain('sr-only');
  });

  it('falls back to gradient text when --primary cannot be parsed', () => {
    document.documentElement.style.setProperty('--primary', 'bogus');
    render(<ParticleTitle text="Logs" />);
    const fallback = screen.getByText('Logs');
    expect(fallback.className).toContain('text-gradient');
    expect(screen.queryByRole('img')).toBeNull();
  });

  it('falls back to gradient text when text is empty', () => {
    const { container } = render(<ParticleTitle text="" />);
    expect(screen.queryByRole('img')).toBeNull();
    const span = container.querySelector('span.text-gradient');
    expect(span).toBeTruthy();
  });
});
