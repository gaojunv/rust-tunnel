// @vitest-environment jsdom
import { describe, expect, it, vi, afterEach } from 'vitest';
import { cleanup, render, screen, fireEvent } from '@testing-library/react';
import MessageBubble from './MessageBubble';
import type { ChatItem } from './types';

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
    // 工具卡片默认收起：先点头部展开，再检查行级折叠（args='{}' 不折叠，result 是页面里第二个 pre）
    fireEvent.click(screen.getByRole('button', { expanded: false }));
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
    fireEvent.click(screen.getByRole('button', { expanded: false }));
    expect(screen.queryByText(/agent.expandLines/)).toBeNull();
    expect(screen.getByText('short output')).toBeTruthy();
  });

  it('collapses long tool args as well', () => {
    const longArgs = Array.from({ length: 12 }, (_, i) => `arg-line-${i}`).join('\n');
    render(
      <MessageBubble item={{ kind: 'tool', content: '', toolName: 'shell', toolArgs: longArgs }} />,
    );
    fireEvent.click(screen.getByRole('button', { expanded: false }));
    expect(screen.getByText(/agent.expandLines/)).toBeTruthy();
  });

  it('renders user and assistant bubbles without collapse button', () => {
    render(<MessageBubble item={{ kind: 'user', content: 'hello' }} />);
    expect(screen.getByText('hello')).toBeTruthy();
    expect(screen.queryByText(/agent.expandLines/)).toBeNull();
  });
});

describe('MessageBubble tool card collapsing', () => {
  afterEach(cleanup);

  it('tool 卡片默认收起，仅显示工具名与摘要', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool',
          content: '',
          toolName: 'read_file',
          toolArgs: '{"path":"src/main.rs"}',
          toolResult: 'pub fn main() {}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('read_file');
    expect(header.textContent).toContain('src/main.rs');
    // 未展开：args 与 result 均不可见
    expect(screen.queryByText('{"path":"src/main.rs"}')).toBeNull();
    expect(screen.queryByText('pub fn main() {}')).toBeNull();
    expect(screen.queryAllByRole('button', { expanded: true })).toHaveLength(0);
  });

  it('点击头部展开完整 args 与 result', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool',
          content: '',
          toolName: 'shell',
          toolArgs: '{"cmd":"ls -la"}',
          toolResult: 'total 4',
        }}
      />,
    );
    fireEvent.click(screen.getByRole('button', { expanded: false }));
    expect(screen.getByRole('button', { expanded: true })).toBeTruthy();
    expect(screen.getByText('{"cmd":"ls -la"}')).toBeTruthy();
    expect(screen.getByText('total 4')).toBeTruthy();
  });

  it('shell 工具摘要显示 cmd', () => {
    render(<MessageBubble item={{ kind: 'tool', content: '', toolName: 'shell', toolArgs: '{"cmd":"ls -la"}' }} />);
    expect(screen.getByRole('button', { expanded: false }).textContent).toContain('ls -la');
  });

  it('search 工具摘要显示 path + pattern', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool',
          content: '',
          toolName: 'search',
          toolArgs: '{"path":"src","pattern":"todo"}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('src');
    expect(header.textContent).toContain('todo');
  });

  it('运行中卡片显示 spinner 状态', () => {
    render(
      <MessageBubble item={{ kind: 'tool', content: '', toolName: 'read_file', toolArgs: '{"path":"x"}' }} />,
    );
    // 折叠态头部即显示 spinner 图标
    const header = screen.getByRole('button', { expanded: false });
    expect(header.querySelector('svg')).toBeTruthy();
    // 展开后显示 toolRunning 文案
    fireEvent.click(header);
    expect(screen.getByText('agent.toolRunning')).toBeTruthy();
  });

  it('ACP execute 工具摘要显示 command（title 风格工具名 + command 字段）', () => {
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'Bash', toolArgs: '{"command":"ls -la","description":"列表"}' }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('ls -la');
  });

  it('ACP edit 工具摘要显示 file_path（title 风格工具名 + file_path 字段）', () => {
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'Edit src/a.ts', toolArgs: '{"file_path":"src/a.ts","old_string":"x"}' }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('src/a.ts');
  });

  it('ACP write 工具摘要显示 file_path（带 toolKind=edit）', () => {
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'Write a.ts', toolKind: 'edit', toolArgs: '{"file_path":"a.ts"}' }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('a.ts');
  });

  it('ACP 命令摘要显示 command，不显示原始 {} 对象', () => {
    render(
      <MessageBubble item={{ kind: 'tool', content: '', toolName: 'Bash', toolArgs: '{"command":"ls"}' }} />,
    );
    const header = screen.getByRole('button', { expanded: false });
    // 摘要应至少包含命令，而不是空/原始 {}
    expect(header.textContent).not.toContain('{}');
    expect(header.textContent).toContain('ls');
  });

  it('空参数 {} 不产生摘要也不显示原始 {}', () => {
    render(
      <MessageBubble item={{ kind: 'tool', content: '', toolName: 'Bash', toolArgs: '{}' }} />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).not.toContain('{}');
    // 展开后详情区也不显示无意义的空对象
    fireEvent.click(header);
    expect(screen.queryByText('{}')).toBeNull();
    expect(screen.queryByText('agent.toolRunning')).toBeTruthy();
  });
});

describe('MessageBubble tool card status badges, plan and thought bubbles', () => {
  afterEach(cleanup);

  const base: ChatItem = { kind: 'tool', content: '', toolName: 'Edit src/a.ts' };

  it('shows status badge: failed', () => {
    render(<MessageBubble item={{ ...base, toolStatus: 'failed', toolResult: 'boom' }} />);
    expect(screen.getByText('✗')).toBeTruthy();
  });

  it('shows completed badge when result present and no explicit status', () => {
    render(<MessageBubble item={{ ...base, toolResult: 'ok' }} />);
    expect(screen.getByText('✓')).toBeTruthy();
  });

  it('shows completed badge when result present even if toolStatus mis-mapped to running', () => {
    // 回归（Bug 3）：ACP 的 ToolCallUpdate 常省略 status，上游可能误映射为
    // running；result 已产出即视为完成，不能显示转圈。
    render(<MessageBubble item={{ ...base, toolStatus: 'running', toolResult: 'ok' }} />);
    expect(screen.getByText('✓')).toBeTruthy();
    expect(screen.queryByText('✗')).toBeNull();
  });

  it('shows running spinner when no result yet (in_progress/running)', () => {
    render(<MessageBubble item={{ ...base, toolStatus: 'running' }} />);
    expect(screen.queryByText('✓')).toBeNull();
    expect(screen.queryByText('✗')).toBeNull();
  });

  it('shows failed badge when status failed despite result', () => {
    render(<MessageBubble item={{ ...base, toolStatus: 'failed', toolResult: 'boom' }} />);
    expect(screen.getByText('✗')).toBeTruthy();
    expect(screen.queryByText('✓')).toBeNull();
  });

  it('renders plan bubble with status markers', () => {
    render(
      <MessageBubble
        item={{
          kind: 'plan',
          content: '',
          planEntries: [
            { content: '第一步', status: 'completed' },
            { content: '第二步', status: 'in_progress' },
            { content: '第三步', status: 'pending' },
          ],
        }}
      />,
    );
    expect(screen.getByText('第一步')).toBeTruthy();
    expect(screen.getByText('第三步')).toBeTruthy();
    expect(screen.getByText('✓')).toBeTruthy();
    expect(screen.getByText('▶')).toBeTruthy();
  });

  it('renders thought bubble collapsed by default', () => {
    render(<MessageBubble item={{ kind: 'thought', content: '内部推理' }} />);
    expect(screen.queryByText('内部推理')).not.toBeTruthy();
  });
});
