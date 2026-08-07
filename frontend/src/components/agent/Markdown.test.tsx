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
    // Streamdown 把 strong 渲染为带 font-semibold 的 span（data-streamdown="strong"）
    expect(screen.getByText('加粗').getAttribute('data-streamdown')).toBe('strong');
    expect(screen.getByText('code').tagName).toBe('CODE');
  });

  it('renders fenced code block with language header and copy button', () => {
    const { container } = render(<Markdown content={'```rust\nfn main() {}\n```'} />);
    // Streamdown 代码块容器带 data-language；body 仍带 language-rust 类
    const block = container.querySelector('[data-streamdown="code-block"]');
    expect(block?.getAttribute('data-language')).toBe('rust');
    expect(container.querySelector('[data-streamdown="code-block-body"]')?.className).toContain('language-rust');
    expect(container.querySelector('[data-streamdown="code-block-copy-button"]')).toBeTruthy();
  });

  it('renders GFM table inside styled wrapper', () => {
    const { container } = render(
      <Markdown content={'| a | b |\n|---|---|\n| 1 | 2 |'} />
    );
    expect(container.querySelector('table')).toBeTruthy();
    expect(container.querySelector('[data-streamdown="table-header-cell"]')).toBeTruthy();
  });
});
