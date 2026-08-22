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

  it('renders fenced code block without nested streamdown frame', () => {
    const { container } = render(<Markdown content={'```rust\nfn main() {}\n```'} />);
    // 修复后代码块不再走 streamdown 默认 block code 的双层边框容器，
    // DOM 中不应再出现 [data-streamdown="code-block"] / code-block-body
    expect(container.querySelector('[data-streamdown="code-block"]')).toBeNull();
    expect(container.querySelector('[data-streamdown="code-block-body"]')).toBeNull();
    // PreFrame 自己的单层框（带 language 头 + copy 按钮，code 内容原样保留）
    expect(screen.getByText('rust')).toBeTruthy();
    expect(screen.getByText('fn main() {}')).toBeTruthy();
  });

  it('inline code still carries font-mono', () => {
    const { container } = render(<Markdown content={'这是 `code`'} />);
    function countFontMono(): number {
      return (container.innerHTML.match(/font-mono/g) ?? []).length;
    }
    const code = screen.getByText('code');
    expect(code.tagName).toBe('CODE');
    // PlainCode 覆盖后行内 code 的 font-mono 由 MD_CLASS 的
    // [&_code:not(pre_code)]:!font-mono 任意变体经父容器 className 施加（jsdom 不
    // 计算 CSS，无法断言计算样式），故断言容器 className 里保留该规则
    expect(countFontMono()).toBeGreaterThan(0);
  });

  it('renders GFM table inside styled wrapper', () => {
    const { container } = render(
      <Markdown content={'| a | b |\n|---|---|\n| 1 | 2 |'} />
    );
    expect(container.querySelector('table')).toBeTruthy();
    expect(container.querySelector('[data-streamdown="table-header-cell"]')).toBeTruthy();
  });

  it('streaming keeps markdown structure (bold) while dropping the code plugin', () => {
    // 问题①回归：流式期间不能裸显 `**` 原文——去掉 code 插件（避免每帧 Shiki
    // 重高亮）但加粗/标题/列表/表格结构仍由 Streamdown 渲染。
    render(<Markdown content={'这是 **加粗** 和 `code`'} streaming />);
    expect(screen.getByText('加粗').getAttribute('data-streamdown')).toBe('strong');
    // 没有 code 插件时行内 code 仍是 code 元素（无高亮 token，不崩溃）
    expect(screen.getByText('code').tagName).toBe('CODE');
  });
});
