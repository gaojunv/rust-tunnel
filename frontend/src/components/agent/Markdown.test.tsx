// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import Markdown from './Markdown';

afterEach(() => {
  cleanup();
});

describe('Markdown', () => {
  it('renders bold and inline code', () => {
    render(<Markdown content={'这是 **加粗** 和 `code`'} />);
    expect(screen.getByText('加粗').tagName).toBe('STRONG');
    expect(screen.getByText('code').tagName).toBe('CODE');
  });

  it('renders fenced code block with language class', () => {
    const { container } = render(<Markdown content={'```rust\nfn main() {}\n```'} />);
    const code = container.querySelector('pre code');
    expect(code).toBeTruthy();
    expect(code?.className).toContain('language-rust');
  });

  it('renders GFM table', () => {
    const { container } = render(
      <Markdown content={'| a | b |\n|---|---|\n| 1 | 2 |'} />
    );
    expect(container.querySelector('table')).toBeTruthy();
  });
});
