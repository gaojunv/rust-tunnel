// @vitest-environment jsdom
import { render } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { GridWaveTitle } from './GridWaveTitle';

// GridWaveTitle 现在只负责渲染标题文字（极光渐变流光）。
// 网格画布已上移到 PageHeader，由 useGridWaveCanvas hook 驱动，
// 因此这里不再测试 canvas 渲染。
describe('GridWaveTitle', () => {
  it('renders the text content', () => {
    const { container } = render(<GridWaveTitle text="Hello" />);
    expect(container.textContent).toContain('Hello');
  });

  it('applies text-aurora class for the gradient effect', () => {
    const { container } = render(<GridWaveTitle text="Hello" />);
    const span = container.querySelector('span');
    expect(span?.className).toContain('text-aurora');
  });

  it('merges custom className', () => {
    const { container } = render(<GridWaveTitle text="Hello" className="custom-cls" />);
    const span = container.querySelector('span');
    expect(span?.className).toContain('custom-cls');
  });
});
