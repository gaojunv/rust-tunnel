// @vitest-environment jsdom
import { describe, expect, it, vi, afterEach } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import SubagentPanel from './SubagentPanel';
import type { SubagentSummary } from './subagent';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const summaries: SubagentSummary[] = [
  {
    index: 1,
    toolId: 'task1',
    label: 'A 任务',
    subagentType: 'general-purpose',
    status: 'in_progress',
    toolCount: 2,
    runningToolLabel: 'Terminal',
  },
  {
    index: 3,
    toolId: 'task2',
    label: 'B 任务',
    status: 'completed',
    toolCount: 1,
    runningToolLabel: null,
  },
];

describe('SubagentPanel', () => {
  afterEach(cleanup);

  it('renders summary rows with status icons and progress', () => {
    render(
      <SubagentPanel variant="top" summaries={summaries} onSelect={vi.fn()} expandedIds={new Set()} />,
    );
    expect(screen.getByText('agent.subagents')).toBeTruthy();
    expect(screen.getByText('A 任务')).toBeTruthy();
    expect(screen.getByText('general-purpose')).toBeTruthy();
    // 运行中进度：N 个工具 · 当前工具（splitToolTitle 归一化 label）
    expect(screen.getByText('2 agent.tools · Terminal')).toBeTruthy();
    expect(screen.getByText('B 任务')).toBeTruthy();
    // 运行中计数徽章（completed 不计入）
    expect(screen.getByText('agent.subagentRunningCount')).toBeTruthy();
  });

  it('calls onSelect with the item index when a row is clicked', () => {
    const onSelect = vi.fn();
    render(
      <SubagentPanel variant="top" summaries={summaries} onSelect={onSelect} expandedIds={new Set()} />,
    );
    fireEvent.click(screen.getAllByTestId('subagent-panel-row')[0]);
    expect(onSelect).toHaveBeenCalledWith(1);
    fireEvent.click(screen.getAllByTestId('subagent-panel-row')[1]);
    expect(onSelect).toHaveBeenCalledWith(3);
  });

  it('highlights expanded rows (linkage with conversation cards)', () => {
    render(
      <SubagentPanel
        variant="top"
        summaries={summaries}
        onSelect={vi.fn()}
        expandedIds={new Set(['task1'])}
      />,
    );
    const rows = screen.getAllByTestId('subagent-panel-row');
    expect(rows[0].className).toContain('bg-accent/40');
    expect(rows[1].className).not.toContain('bg-accent/40');
  });

  it('collapses and expands via header toggle (top variant)', () => {
    render(
      <SubagentPanel variant="top" summaries={summaries} onSelect={vi.fn()} expandedIds={new Set()} />,
    );
    expect(screen.getAllByTestId('subagent-panel-row')).toHaveLength(2);
    fireEvent.click(screen.getByRole('button', { name: 'agent.subagentCollapse' }));
    expect(screen.queryAllByTestId('subagent-panel-row')).toHaveLength(0);
    fireEvent.click(screen.getByRole('button', { name: 'agent.subagentExpand' }));
    expect(screen.getAllByTestId('subagent-panel-row')).toHaveLength(2);
  });

  it('collapses sidebar to a narrow icon bar and back', () => {
    render(
      <SubagentPanel variant="sidebar" summaries={summaries} onSelect={vi.fn()} expandedIds={new Set()} />,
    );
    expect(screen.getAllByTestId('subagent-panel-row')).toHaveLength(2);
    fireEvent.click(screen.getByRole('button', { name: 'agent.subagentCollapse' }));
    expect(screen.queryAllByTestId('subagent-panel-row')).toHaveLength(0);
    fireEvent.click(screen.getByRole('button', { name: 'agent.subagentExpand' }));
    expect(screen.getAllByTestId('subagent-panel-row')).toHaveLength(2);
  });
});
