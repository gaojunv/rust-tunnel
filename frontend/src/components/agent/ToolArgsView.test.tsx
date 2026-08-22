// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { ToolArgsView, ToolResultView } from './ToolArgsView';
import '../../i18n';

describe('ToolArgsView', () => {
  afterEach(cleanup);

  it('execute: renders shell command block (heredoc retains newlines, no JSON braces)', () => {
    const cmd = 'git commit -m "$(cat <<\'EOF\'\nfix foo\nEOF\n)"';
    const { container } = render(
      <ToolArgsView name="Bash" kind="execute" args={JSON.stringify({ command: cmd, description: '提交' })} />,
    );
    expect(screen.getByText('提交')).toBeTruthy();
    const pre = container.querySelector('pre');
    expect(pre).toBeTruthy();
    expect(pre?.textContent).toBe(cmd);
    // 结构化展示，不应再出现 raw JSON 的引号包裹 key
    expect(pre?.textContent).not.toContain('{"command"');
    // 多行 heredoc 原样换行
    expect(pre?.textContent).toContain('fix foo\nEOF');
  });

  it('execute: falls back to generic when no cmd/command', () => {
    const { container } = render(
      <ToolArgsView name="Bash" kind="execute" args={JSON.stringify({ description: '提交' })} />,
    );
    expect(screen.queryByText('提交')).toBeTruthy();
    expect(container.textContent ?? '').toContain('提交');
  });

  it('invalid JSON falls back to CollapsiblePre (original text)', () => {
    render(<ToolArgsView args={'{not json'} />);
    expect(screen.getByText('{not json')).toBeTruthy();
  });

  it('non-object JSON falls back to CollapsiblePre', () => {
    render(<ToolArgsView args={'[1,2,3]'} />);
    expect(screen.getByText('[1,2,3]')).toBeTruthy();
  });

  it('generic kv: short string on one line', () => {
    const { container } = render(
      <ToolArgsView args={JSON.stringify({ path: 'src/main.rs', limit: '20' })} />,
    );
    expect(container.textContent ?? '').toContain('path');
    expect(container.textContent ?? '').toContain('src/main.rs');
  });
});

describe('ToolResultView', () => {
  afterEach(cleanup);

  it('execute: terminal dark block class', () => {
    const { container } = render(
      <ToolResultView name="Bash" kind="execute" result={'ok\nline2'} />,
    );
    const pre = container.querySelector('pre');
    expect(pre).toBeTruthy();
    expect(pre?.className).toContain('bg-zinc-950');
    expect(pre?.className).toContain('text-zinc-100');
  });

  it('read: marker caption + gutter starting from marker', () => {
    const result = 'fn main() {}\nlet x = 1;\n[showing lines 10-11 of 100]';
    const { container } = render(
      <ToolResultView name="Read" kind="read" args={'{"file_path":"a.ts","offset":"999"}'} result={result} />,
    );
    // caption 来自 marker，不是 args 的 offset
    expect(container.textContent ?? '').toContain('10-11');
    // gutter 行号从 10 起
    expect(container.textContent ?? '').not.toContain('[showing lines');
    expect(screen.getByText('10')).toBeTruthy();
    expect(screen.getByText('11')).toBeTruthy();
    expect(screen.queryByText('999')).toBeNull();
  });

  it('read: ACP-prefixed lines passthrough without duplicate gutter', () => {
    const result = '  12→fn main() {}\n  13→let x = 1;';
    const { container } = render(<ToolResultView name="Read" kind="read" result={result} />);
    expect(container.textContent ?? '').toContain('fn main()');
    expect(container.querySelector('.w-10')).toBeNull();
  });

  it('read: claude-code tab-separated line numbers passthrough (no double gutter)', () => {
    // claude-code Read 的真实输出是「空格 + 行号 + Tab」（终端把 Tab 显示成 →），
    // 只认 →/│/| 会漏判而再叠一层 gutter（双行号回归）
    const result = '     1\tfn main() {}\n     2\tlet x = 1;';
    const { container } = render(
      <ToolResultView name="Read" kind="read" args={'{"file_path":"a.ts","offset":1}'} result={result} />,
    );
    expect(container.textContent ?? '').toContain('fn main()');
    // 无 gutter（.w-10 是 gutter 的定宽类）
    expect(container.querySelector('.w-10')).toBeNull();
  });

  it('read: marker caption kept even when content is ACP-prefixed', () => {
    const result = '  10→fn main() {}\n  11→let x = 1;\n[showing lines 10-11 of 100]';
    const { container } = render(<ToolResultView name="Read" kind="read" result={result} />);
    expect(container.textContent ?? '').toContain('10-11');
    expect(container.textContent ?? '').not.toContain('[showing lines');
    expect(container.querySelector('.w-10')).toBeNull();
  });

  it('read: non-prefixed plain content with offset still gets gutter', () => {
    // 确认多数确认规则不误伤：普通代码 + offset → 仍加 gutter
    const result = 'fn main() {}\nlet x = 1;\nreturn 0;';
    const { container } = render(
      <ToolResultView name="Read" kind="read" args={'{"file_path":"a.ts","offset":20}'} result={result} />,
    );
    expect(container.querySelector('.w-10')).toBeTruthy();
    expect(screen.getByText('20')).toBeTruthy();
  });

  it('search: code-block style with result text', () => {
    render(<ToolResultView name="search" kind="search" result={'src/a.ts:3: todo fix'} />);
    expect(screen.getByText('src/a.ts:3: todo fix')).toBeTruthy();
  });

  it('other kind: CollapsiblePre with text', () => {
    render(<ToolResultView result={'hello world'} />);
    expect(screen.getByText('hello world')).toBeTruthy();
  });
});
