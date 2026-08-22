// @vitest-environment jsdom
import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import ToolDiffView from './ToolDiffView';
import '../../i18n';

describe('ToolDiffView', () => {
  it('renders removed/added/context lines with marker classes', () => {
    const { container } = render(
      <ToolDiffView
        diffs={[{ path: 'src/a.ts', old_text: 'keep\nold line', new_text: 'keep\nnew line' }]}
      />,
    );
    expect(screen.getByText('src/a.ts')).toBeTruthy();
    // 移动 unified 视图（pre.md:hidden）内：+ - 前缀仍存在，上下文行无标记类
    const pre = container.querySelector('pre.md\\:hidden');
    expect(pre).toBeTruthy();
    expect(pre?.textContent).toContain('+ new line');
    expect(pre?.textContent).toContain('- old line');
    // unified 容器内：context 行是空前缀行
    const ctx = Array.from(pre!.querySelectorAll('div')).find(
      (d) => d.textContent === '  keep',
    );
    expect(ctx).toBeTruthy();
    expect(ctx?.className ?? '').not.toContain('diff-line-add');
    expect(ctx?.className ?? '').not.toContain('diff-line-del');
  });

  it('renders new-file (old_text null) as all-added', () => {
    const { container } = render(
      <ToolDiffView diffs={[{ path: 'new.ts', old_text: null, new_text: 'a\nb' }]} />,
    );
    // 新结构：桌面双栏与移动 unified 各一份，所以统计以 unified pre 为准
    const pre = container.querySelector('pre.md\\:hidden');
    expect(pre).toBeTruthy();
    expect(pre!.querySelectorAll('.diff-line-add')).toHaveLength(2);
    expect(pre!.querySelector('.diff-line-del')).toBeNull();
  });

  it('renders removed-file (new_text null) as all-removed', () => {
    const { container } = render(
      <ToolDiffView diffs={[{ path: 'gone.ts', old_text: 'x', new_text: null }]} />,
    );
    const pre = container.querySelector('pre.md\\:hidden');
    expect(pre).toBeTruthy();
    expect(pre!.querySelectorAll('.diff-line-del')).toHaveLength(1);
  });

  it('pairs del/add replacement in the same grid row (desktop two-pane)', () => {
    const { container } = render(
      <ToolDiffView
        diffs={[{ path: 'p.ts', old_text: 'hello', new_text: 'world' }]}
      />,
    );
    // 桌面视图：单一 grid-cols-2 容器 + 扁平格子（左格带 border-r），替换行的
    // del/add 应相邻排列（同行左右对齐：左格 'hello' 紧跟右格 'world'）
    const grid = container.querySelector('.grid.grid-cols-2');
    expect(grid).toBeTruthy();
    const cells = Array.from(grid!.children) as HTMLElement[];
    const delIdx = cells.findIndex((c) => c.textContent?.includes('hello'));
    expect(delIdx).toBeGreaterThanOrEqual(0);
    expect(cells[delIdx + 1]?.textContent).toContain('world');
    expect(cells[delIdx].className).toContain('diff-line-del');
    expect(cells[delIdx + 1].className).toContain('diff-line-add');
  });

  it('has responsive class split (desktop hidden md:block, mobile md:hidden)', () => {
    const { container } = render(
      <ToolDiffView diffs={[{ path: 'p.ts', old_text: 'a', new_text: 'b' }]} />,
    );
    expect(container.querySelector('.hidden.md\\:block')).toBeTruthy();
    expect(container.querySelector('.md\\:hidden')).toBeTruthy();
  });
});
