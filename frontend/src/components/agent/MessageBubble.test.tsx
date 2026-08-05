// @vitest-environment jsdom
import { describe, expect, it, vi, afterEach } from 'vitest';
import { cleanup, render, screen, fireEvent } from '@testing-library/react';
import MessageBubble from './MessageBubble';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string, opts?: { count?: number }) => (opts?.count ? `${k}:${opts.count}` : k) }),
}));

describe('MessageBubble tool output collapsing', () => {
  afterEach(cleanup);

  const longResult = Array.from({ length: 20 }, (_, i) => `line-${i + 1}`).join('\n');

  it('collapses tool result to first 3 lines when long, expands on click', () => {
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'shell', toolArgs: '{}', toolResult: longResult }}
      />,
    );
    // 折叠态：只显示前 3 行（args='{}' 不折叠，result 是页面里第二个 pre）
    const pres = document.querySelectorAll('pre');
    const resultPre = pres[pres.length - 1];
    expect(resultPre.textContent).toBe('line-1\nline-2\nline-3');
    expect(resultPre.textContent).not.toContain('line-4');
    // 展开按钮显示剩余行数
    const btn = screen.getByText('agent.expandLines:17');
    fireEvent.click(btn);
    // 展开后全文可见
    expect(document.querySelectorAll('pre')[pres.length - 1].textContent).toContain('line-20');
    // 再点收起
    fireEvent.click(screen.getByText('agent.collapse'));
    expect(document.querySelectorAll('pre')[pres.length - 1].textContent).not.toContain('line-4');
  });

  it('does not collapse short tool output', () => {
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'shell', toolArgs: '{}', toolResult: 'short output' }}
      />,
    );
    expect(screen.queryByText(/agent.expandLines/)).toBeNull();
    expect(screen.getByText('short output')).toBeTruthy();
  });

  it('collapses long tool args as well', () => {
    const longArgs = Array.from({ length: 12 }, (_, i) => `arg-line-${i}`).join('\n');
    render(
      <MessageBubble item={{ kind: 'tool', content: '', toolName: 'shell', toolArgs: longArgs }} />,
    );
    expect(screen.getByText(/agent.expandLines/)).toBeTruthy();
  });

  it('renders user and assistant bubbles without collapse button', () => {
    render(<MessageBubble item={{ kind: 'user', content: 'hello' }} />);
    expect(screen.getByText('hello')).toBeTruthy();
    expect(screen.queryByText(/agent.expandLines/)).toBeNull();
  });
});
