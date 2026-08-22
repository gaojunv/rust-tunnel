// @vitest-environment jsdom
import { describe, expect, it, vi, afterEach } from 'vitest';
import { cleanup, render, screen, fireEvent } from '@testing-library/react';
import MessageBubble, { CollapsiblePre, effectiveToolKind, resolveToolStatus, splitToolTitle } from './MessageBubble';
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

describe('CollapsiblePre character limit protection', () => {
  afterEach(cleanup);

  it('truncates a single-line text longer than MAX_CHARS (no newlines) with an expand button', () => {
    const longLine = 'a'.repeat(9000);
    render(<CollapsiblePre text={longLine} />);
    const pre = document.querySelector('pre');
    // 前 8000 字符 + 省略标记：超长单行（无换行，行数=1 不触发行折叠）也被字符保护截断
    expect(pre?.textContent).toHaveLength(8001);
    expect(pre?.textContent).toContain('a'.repeat(8000));
    expect(pre?.textContent).not.toContain('a'.repeat(8001));
    // 按钮文案提示字符总量
    expect(screen.getByText('agent.expandChars:9000')).toBeTruthy();
  });

  it('char truncation takes precedence over line folding for long multi-line text', () => {
    const line = 'z'.repeat(100);
    const multi = Array.from({ length: 120 }, () => line).join('\n');
    expect(multi.length).toBeGreaterThan(8000);
    render(<CollapsiblePre text={multi} />);
    const pre = document.querySelector('pre');
    expect(pre?.textContent).toHaveLength(8001);
    expect(screen.getByText(`agent.expandChars:${multi.length}`)).toBeTruthy();
  });

  it('renders normal short text fully without a button', () => {
    render(<CollapsiblePre text="hello world" />);
    expect(screen.getByText('hello world')).toBeTruthy();
    expect(screen.queryByText(/agent\.(expandLines|expandChars|collapse)/)).toBeNull();
  });

  it('expanding reveals the full text, collapsing returns to the truncated view', () => {
    const longLine = 'a'.repeat(9000);
    render(<CollapsiblePre text={longLine} />);
    expect(document.querySelector('pre')?.textContent).toHaveLength(8001);
    fireEvent.click(screen.getByText('agent.expandChars:9000'));
    // 展开后显示完整文本（无省略标记）
    expect(document.querySelector('pre')?.textContent).toBe(longLine);
    fireEvent.click(screen.getByText('agent.collapse'));
    expect(document.querySelector('pre')?.textContent).toHaveLength(8001);
  });

  it('char truncation does not split a surrogate pair (no garbled breakpoint)', () => {
    const emoji = '😀'; // U+1F600 = 😀，占 2 个 UTF-16 码元
    const text = 'a'.repeat(7999) + emoji + 'b'.repeat(100);
    render(<CollapsiblePre text={text} />);
    const shown = document.querySelector('pre')?.textContent ?? '';
    expect(shown.endsWith('…')).toBe(true);
    // 截断点回退到代理对之前：省略号前一个码元不是孤立代理（无乱码半字符）
    const beforeEllipsis = shown.charCodeAt(shown.length - 2);
    expect(beforeEllipsis >= 0xd800 && beforeEllipsis <= 0xdfff).toBe(false);
    // 完整 emoji 未显示（回退丢弃整个代理对，避免截出半截）
    expect(shown).not.toContain(emoji);
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
    // runner 旧格式工具名 read_file 归一化为规范名 Read；路径只显示文件名
    expect(header.textContent).toContain('Read');
    expect(header.textContent).toContain('main.rs');
    expect(header.textContent).not.toContain('src/main.rs');
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
    // execute 类：ToolArgsView 结构化为命令块（不再曝露 raw JSON——头部摘要已含 ls -la，展开区又有一份命令块，所以 getAllByText）。
    expect(screen.getAllByText('ls -la').length).toBeGreaterThanOrEqual(1);
    expect(screen.queryByText('{"cmd":"ls -la"}')).toBeNull();
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

  it('ACP edit 工具摘要显示 file_path（title 风格工具名 + file_path 字段），路径只显示文件名', () => {
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'Edit src/a.ts', toolArgs: '{"file_path":"src/a.ts","old_string":"x"}' }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('a.ts');
    expect(header.textContent).not.toContain('src/a.ts');
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

  it('code_outline 工具显示为 Outline + 文件名摘要', () => {
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'code_outline', toolArgs: '{"path":"src/main.rs"}' }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('Outline');
    expect(header.textContent).toContain('main.rs');
  });

  it('read_symbol 工具显示为 Symbol + basename › name 摘要', () => {
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'read_symbol', toolArgs: '{"path":"src/main.rs","name":"main"}' }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('Symbol');
    expect(header.textContent).toContain('main.rs › main');
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

  it('title 内嵌相对路径与 args 绝对路径去重，只显示一份（basename）', () => {
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
    expect(header.textContent).not.toContain('src/a.ts');
    expect(header.textContent?.match(/a\.ts/g)).toHaveLength(1);
  });

  it('Edit args 与 title 均无路径时回退到 diffs 路径', () => {
    // 回归（用户反馈 Bug）：ACP Edit 的 raw_input 为空占位（args='{}'）、title
    // 只是别名（无内嵌路径），路径只经 content Diff 到达——头部摘要必须显示
    // diffs 的目标文件，否则卡片只剩「Edit」。
    render(
      <MessageBubble
        item={{
          kind: 'tool',
          content: '',
          toolName: 'Edit',
          toolKind: 'edit',
          toolArgs: '{}',
          toolDiffs: [{ path: 'src/a.ts', old_text: 'x', new_text: 'y' }],
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('Edit');
    expect(header.textContent).toContain('a.ts');
    expect(header.textContent).not.toContain('src/a.ts');
  });

  it('Edit args 与 title 均无路径时回退到 locations 路径', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool',
          content: '',
          toolName: 'Edit',
          toolKind: 'edit',
          toolArgs: '{}',
          toolLocations: [{ path: 'src/b.ts', line: 3 }],
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('Edit');
    expect(header.textContent).toContain('b.ts');
    expect(header.textContent).not.toContain('src/b.ts');
  });

  it('args/title/diffs/locations 均无路径时头部仅显示工具名，不崩溃', () => {
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'Edit', toolKind: 'edit', toolArgs: '{}' }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent?.trim()).toBe('Edit');
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

  it('keeps spinner when toolStatus explicitly running even with a result (subagent mid-state)', () => {
    // 回归（问题②）：Task 父卡的中间态 ToolCallUpdate 带部分输出（status=running），
    // 显式 running 优先于 result 推断——子 agent 未执行完不能提前打勾。Bug 3 的
    // 误映射已由服务端修复（普通工具 status 缺失 + result → completed），前端不再
    // 需要用 result 覆盖显式 running。
    render(<MessageBubble item={{ ...base, toolStatus: 'running', toolResult: 'ok' }} />);
    expect(screen.queryByText('✓')).toBeNull();
    expect(screen.queryByText('✗')).toBeNull();
    // 状态徽章是转圈图标（运行中）
    const header = screen.getByRole('button', { expanded: false });
    expect(header.querySelector('svg')).toBeTruthy();
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

  it('空结果 + 终态状态不误显「执行中」（M5：按 toolStatus 门控）', () => {
    // 服务端新契约 tool_result JSON status=completed/failed 且 text 为空串时，
    // 详情区不应再显示「执行中」——状态已是终态，spinner 只在运行中显示。
    const { unmount } = render(
      <MessageBubble item={{ ...base, toolStatus: 'completed', toolResult: '' }} />,
    );
    fireEvent.click(screen.getByRole('button', { expanded: false }));
    expect(screen.queryByText('agent.toolRunning')).toBeNull();
    unmount();
    render(<MessageBubble item={{ ...base, toolStatus: 'failed', toolResult: '' }} />);
    fireEvent.click(screen.getByRole('button', { expanded: false }));
    expect(screen.queryByText('agent.toolRunning')).toBeNull();
    expect(screen.getByText('✗')).toBeTruthy();
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

describe('splitToolTitle runner tool name mapping', () => {
  it('maps read_file to Read, list_dir to List, code_outline to Outline, read_symbol to Symbol', () => {
    expect(splitToolTitle('read_file', undefined)).toEqual({ label: 'Read', extra: null });
    expect(splitToolTitle('list_dir', undefined)).toEqual({ label: 'List', extra: null });
    expect(splitToolTitle('code_outline', undefined)).toEqual({ label: 'Outline', extra: null });
    expect(splitToolTitle('read_symbol', undefined)).toEqual({ label: 'Symbol', extra: null });
  });

  it('maps git_* tools to Git label', () => {
    expect(splitToolTitle('git_status', undefined)).toEqual({ label: 'Git', extra: null });
    expect(splitToolTitle('git_diff', undefined)).toEqual({ label: 'Git', extra: null });
    expect(splitToolTitle('git_commit', undefined)).toEqual({ label: 'Git', extra: null });
  });

  it('maps write_file to Write, patch_file to Patch', () => {
    expect(splitToolTitle('write_file', undefined)).toEqual({ label: 'Write', extra: null });
    expect(splitToolTitle('patch_file', undefined)).toEqual({ label: 'Patch', extra: null });
  });

  it('maps shell to Terminal, search to Search', () => {
    expect(splitToolTitle('shell', undefined)).toEqual({ label: 'Terminal', extra: null });
    expect(splitToolTitle('search', undefined)).toEqual({ label: 'Search', extra: null });
  });

  it('maps todo_write to Todo, remember to Remember, use_skill to Skill, task to Task', () => {
    expect(splitToolTitle('todo_write', undefined)).toEqual({ label: 'Todo', extra: null });
    expect(splitToolTitle('remember', undefined)).toEqual({ label: 'Remember', extra: null });
    expect(splitToolTitle('use_skill', undefined)).toEqual({ label: 'Skill', extra: null });
    expect(splitToolTitle('task', undefined)).toEqual({ label: 'Task', extra: null });
  });

  it('explicit toolKind takes precedence over RUNNER_TOOL_META', () => {
    // ACP 路径带显式 kind：优先匹配 KIND_META，不受 RUNNER_TOOL_META 影响
    expect(splitToolTitle('read_file', 'read')).toEqual({ label: 'Read', extra: null });
    expect(splitToolTitle('git_diff', 'read')).toEqual({ label: 'Read', extra: 'git_diff' });
  });

  it('unknown tool name returns raw name', () => {
    expect(splitToolTitle('custom_tool_xyz', undefined)).toEqual({ label: 'custom_tool_xyz', extra: null });
  });
});

describe('effectiveToolKind', () => {
  it('returns explicit toolKind when not other', () => {
    expect(effectiveToolKind('read_file', 'read')).toBe('read');
    expect(effectiveToolKind('shell', 'execute')).toBe('execute');
  });

  it('infers kind from RUNNER_TOOL_META when kind is undefined', () => {
    expect(effectiveToolKind('read_file', undefined)).toBe('read');
    expect(effectiveToolKind('shell', undefined)).toBe('execute');
    expect(effectiveToolKind('git_status', undefined)).toBe('other');
    expect(effectiveToolKind('todo_write', undefined)).toBe('think');
  });

  it('falls back to other for unknown tools', () => {
    expect(effectiveToolKind('unknown_tool', undefined)).toBe('other');
    expect(effectiveToolKind('unknown_tool', 'other')).toBe('other');
  });
});

describe('toolSummary enhanced for runner tools', () => {
  afterEach(cleanup);

  it('read_file with offset and limit shows path:offset-end', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool', content: '', toolName: 'read_file',
          toolArgs: '{"path":"src/main.rs","offset":10,"limit":20}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('main.rs:10-29');
  });

  it('read_file with offset only shows path:offset-', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool', content: '', toolName: 'read_file',
          toolArgs: '{"path":"src/main.rs","offset":5}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('main.rs:5-');
  });

  it('read_file with limit only shows path:1-end (offset defaults to 1)', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool', content: '', toolName: 'read_file',
          toolArgs: '{"path":"src/main.rs","limit":10}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('main.rs:1-10');
  });

  it('read_file without offset/limit shows path only (basename)', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool', content: '', toolName: 'read_file',
          toolArgs: '{"path":"src/main.rs"}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('main.rs');
    expect(header.textContent).not.toContain(':');
  });

  it('read_file accepts numeric string offset/limit (LLM 有时传 "120")', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool', content: '', toolName: 'read_file',
          toolArgs: '{"path":"src/main.rs","offset":"10","limit":"20"}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('main.rs:10-29');
  });

  it('read_symbol shows path › name summary', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool', content: '', toolName: 'read_symbol',
          toolArgs: '{"path":"src/main.rs","name":"main"}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    // 摘要是 "src/main.rs › main"，文件工具只显示 basename
    expect(header.textContent).toContain('main.rs › main');
  });

  it('git_diff shows file_path summary', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool', content: '', toolName: 'git_diff',
          toolArgs: '{"file_path":"src/main.rs"}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('main.rs');
  });

  it('git_checkout shows name summary', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool', content: '', toolName: 'git_checkout',
          toolArgs: '{"name":"feature-branch"}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('feature-branch');
  });

  it('git_branch shows name summary', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool', content: '', toolName: 'git_branch',
          toolArgs: '{"name":"main"}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('main');
  });

  it('git_show shows rev summary', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool', content: '', toolName: 'git_show',
          toolArgs: '{"rev":"abc123"}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('abc123');
  });

  it('git_* unknown subcommand returns null summary (header shows label only)', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool', content: '', toolName: 'git_status',
          toolArgs: '{}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('Git');
  });

  it('toolKind inferred by effectiveToolKind for git_* shows Git chip', () => {
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'git_commit', toolArgs: '{}' }}
      />,
    );
    // git_* 工具 kind=other，但 effectiveToolKind 返回 other（Git label 来自 RUNNER_TOOL_META）
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('Git');
  });
});

describe('resolveToolStatus explicit status priority', () => {
  it('explicit running + result stays running (subagent mid-state, no early checkmark)', () => {
    // 回归（问题②）：Task 父卡中间态 ToolCallUpdate 带部分输出（status=running），
    // toolResult 非空不能覆盖成 completed——否则子 agent 没执行完就打勾。
    expect(
      resolveToolStatus({ kind: 'tool', content: '', toolStatus: 'running', toolResult: 'partial' }),
    ).toBe('running');
  });

  it('explicit in_progress / pending stay explicit', () => {
    expect(
      resolveToolStatus({ kind: 'tool', content: '', toolStatus: 'in_progress', toolResult: 'x' }),
    ).toBe('in_progress');
    expect(resolveToolStatus({ kind: 'tool', content: '', toolStatus: 'pending' })).toBe('pending');
  });

  it('explicit completed / failed win over result', () => {
    expect(resolveToolStatus({ kind: 'tool', content: '', toolStatus: 'completed' })).toBe('completed');
    expect(
      resolveToolStatus({ kind: 'tool', content: '', toolStatus: 'failed', toolResult: 'boom' }),
    ).toBe('failed');
  });

  it('missing status infers completed from result (keep legacy inference)', () => {
    // Bug 3 回归保护：toolStatus 缺省 + result → completed，不能转圈
    expect(resolveToolStatus({ kind: 'tool', content: '', toolResult: 'ok' })).toBe('completed');
  });

  it('missing status and no result infers in_progress', () => {
    expect(resolveToolStatus({ kind: 'tool', content: '' })).toBe('in_progress');
  });
});

describe('MessageBubble PathTip 完整路径提示', () => {
  afterEach(cleanup);

  const fileItem: ChatItem = {
    kind: 'tool',
    content: '',
    toolName: 'Read File',
    toolKind: 'read',
    toolArgs: '{"file_path":"/home/u/proj/src/main.rs"}',
  };

  it('文件工具头部只显示 basename，鼠标悬浮显示完整路径', () => {
    render(<MessageBubble item={fileItem} />);
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('main.rs');
    expect(header.textContent).not.toContain('/home/u/proj');

    fireEvent.mouseEnter(screen.getByText('main.rs'));
    expect(screen.getByRole('tooltip').textContent).toBe('/home/u/proj/src/main.rs');
  });

  it('点击（模拟触摸）路径切换显示完整路径，点击外部关闭', () => {
    render(<MessageBubble item={fileItem} />);
    // 触摸点击路径：弹出完整路径 tip
    fireEvent.click(screen.getByText('main.rs'));
    expect(screen.getByRole('tooltip').textContent).toBe('/home/u/proj/src/main.rs');
    // 点击外部任意处关闭
    fireEvent.pointerDown(document.body);
    expect(screen.queryByRole('tooltip')).toBeNull();
  });

  it('点击路径不展开工具卡片（stopPropagation）', () => {
    render(<MessageBubble item={fileItem} />);
    fireEvent.click(screen.getByText('main.rs'));
    expect(screen.getByRole('button', { expanded: false })).toBeTruthy();
    expect(screen.queryByRole('button', { expanded: true })).toBeNull();
  });

  it('相对路径同样只显示文件名', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool',
          content: '',
          toolName: 'Edit a.ts',
          toolKind: 'edit',
          toolArgs: '{"file_path":"src/components/a.ts"}',
        }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('a.ts');
    expect(header.textContent).not.toContain('src/components');
  });

  it('非文件工具（execute）摘要保留完整命令，不生成 PathTip', () => {
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'Bash', toolKind: 'execute', toolArgs: '{"command":"ls -la /home/u"}' }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('ls -la /home/u');
    expect(screen.queryByRole('tooltip')).toBeNull();
  });

  it('search 摘要保留 path ⌕ pattern，不做 basename 处理', () => {
    render(
      <MessageBubble
        item={{ kind: 'tool', content: '', toolName: 'search', toolArgs: '{"path":"src","pattern":"todo"}' }}
      />,
    );
    const header = screen.getByRole('button', { expanded: false });
    expect(header.textContent).toContain('src ⌕ todo');
    expect(screen.queryByRole('tooltip')).toBeNull();
  });
});

describe('MessageBubble 工具卡片参数/结果区分展示', () => {
  afterEach(cleanup);

  it('read 工具展开后隐藏 args（摘要已表达）、只显示结果', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool',
          content: '',
          toolName: 'Read File',
          toolKind: 'read',
          toolArgs: '{"file_path":"src/main.rs"}',
          toolResult: 'pub fn main() {}',
          toolStatus: 'completed',
        }}
      />,
    );
    fireEvent.click(screen.getByRole('button', { expanded: false }));
    // 展开区不显示 args JSON（含 file_path），但显示结果
    expect(screen.queryByText(/"file_path"/)).toBeNull();
    expect(screen.getByText('pub fn main() {}')).toBeTruthy();
  });

  it('edit+diffs 后不显示 args JSON（diff 已覆盖）', () => {
    render(
      <MessageBubble
        item={{
          kind: 'tool',
          content: '',
          toolName: 'Edit a.ts',
          toolKind: 'edit',
          toolArgs: '{"file_path":"a.ts","old_string":"x"}',
          toolDiffs: [{ path: 'a.ts', old_text: 'x', new_text: 'y' }],
          toolResult: 'ok',
          toolStatus: 'completed',
        }}
      />,
    );
    fireEvent.click(screen.getByRole('button', { expanded: false }));
    // 展开区是 diff 视图 + 结果，不再曝露工具参数 JSON
    expect(screen.queryByText(/old_string/)).toBeNull();
    expect(screen.getAllByText('a.ts').length).toBeGreaterThanOrEqual(1);
  });

  it('shell 展开后显示结构化命令（非 raw JSON，heredoc 保留换行）', () => {
    const cmd = 'git commit -m "$(cat <<\'EOF\'\nfix\nEOF\n)"';
    render(
      <MessageBubble
        item={{
          kind: 'tool',
          content: '',
          toolName: 'shell',
          toolKind: 'execute',
          toolArgs: JSON.stringify({ cmd, description: '提交' }),
          toolResult: 'ok',
          toolStatus: 'completed',
        }}
      />,
    );
    fireEvent.click(screen.getByRole('button', { expanded: false }));
    expect(screen.getByText('提交')).toBeTruthy();
    // 命令块是 <pre> 原样文本，Testing Library 会 normalize 空白导致多行匹配失效，用 textContent 断言
    const text = document.body.textContent ?? '';
    expect(text).toContain(cmd);
    expect(text).not.toContain('"cmd"');
  });
});
