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
    expect(container.querySelector('.diff-line-add')?.textContent).toBe('+ new line');
    expect(container.querySelector('.diff-line-del')?.textContent).toBe('- old line');
    // 上下文行无标记类
    const ctx = Array.from(container.querySelectorAll('div')).find(
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
    expect(container.querySelectorAll('.diff-line-add')).toHaveLength(2);
    expect(container.querySelector('.diff-line-del')).toBeNull();
  });

  it('renders removed-file (new_text null) as all-removed', () => {
    const { container } = render(
      <ToolDiffView diffs={[{ path: 'gone.ts', old_text: 'x', new_text: null }]} />,
    );
    expect(container.querySelectorAll('.diff-line-del')).toHaveLength(1);
  });
});
