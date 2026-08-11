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
    // runner 旧格式工具名 read_file 归一化为规范名 Read
    expect(header.textContent).toContain('Read');
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

  it('标题归一化：Read File / Read 均显示为规范名 Read', () => {
    const { unmount } = render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'Read File', toolKind: 'read', toolArgs: '{}' }}
      />,
    );
    expect(screen.getByRole('button', { expanded: false }).textContent).toBe('Read');
    unmount();
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'Read', toolKind: 'read', toolArgs: '{}' }}
      />,
    );
    expect(screen.getByRole('button', { expanded: false }).textContent).toBe('Read');
  });

  it('标题归一化：execute 的 title 为命令本体时显示 Terminal + 命令摘要', () => {
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'npm test', toolKind: 'execute', toolArgs: '{}' }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('Terminal');
    expect(header.textContent).toContain('npm test');
  });

  it('标题归一化：execute 命令以 run 开头时不被误剥离', () => {
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'run npm test', toolKind: 'execute', toolArgs: '{}' }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('run npm test');
  });

  it('title 内嵌相对路径与 args 绝对路径去重，只显示一份', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool',
          content: '',
          toolName: 'Edit src/a.ts',
          toolKind: 'edit',
          toolArgs: '{"file_path":"/home/u/proj/src/a.ts"}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('Edit');
    expect(header.textContent?.match(/src\/a\.ts/g)).toHaveLength(1);
  });

  it('进度条容器在完成后仍占位（不卸载），仅淡出', () => {
    const { container, rerender } = render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'Bash', toolKind: 'execute', toolArgs: '{}' }}
      />,
    );
    const barBefore = container.querySelector('.h-0\\.5');
    expect(barBefore).toBeTruthy();
    expect(barBefore?.className).toContain('bg-muted');
    rerender(
      <MessageBubble
        item={{
          kind: 'tool',
          content: '',
          toolName: 'Bash',
          toolKind: 'execute',
          toolArgs: '{}',
          toolResult: 'done',
        }}
      />,
    );
    const barAfter = container.querySelector('.h-0\\.5');
    // 容器常驻（避免高度跳变），背景淡出为透明，动画条移除
    expect(barAfter).toBeTruthy();
    expect(barAfter?.className).toContain('bg-transparent');
    expect(container.querySelector('.animate-pulse')).toBeNull();
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

  it('renders thought card collapsed by default with one-line preview', () => {
    render(<MessageBubble item={{ kind: 'thought', content: '## 计划\n先分析再动手' }} />);
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('agent.thought');
    // 折叠态头部显示首行预览（剥掉 md 标题标记），完整内容不渲染
    expect(header.textContent).toContain('计划');
    expect(screen.queryByText('先分析再动手')).toBeNull();
  });

  it('expanding thought card renders content as Markdown', () => {
    render(<MessageBubble item={{ kind: 'thought', content: '这是 **加粗** 的思考' }} />);
    fireEvent.click(screen.getByRole('button', { expanded: false }));
    expect(screen.getByText('加粗').getAttribute('data-streamdown')).toBe('strong');
  });

  it('thought preview strips inline emphasis markers', () => {
    render(<MessageBubble item={{ kind: 'thought', content: '用 **方案A** 实现 `foo` 函数' }} />);
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('用 方案A 实现 foo 函数');
    expect(header.textContent).not.toContain('**');
  });
});
