// @vitest-environment jsdom
import { describe, expect, it, vi, afterEach } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import SubagentTaskCard from './SubagentTaskCard';
import type { ChatItem } from './types';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({
    t: (k: string) => (k === 'agent.tools' ? '个工具' : k),
  }),
}));

const childTool = (
  toolId: string,
  overrides: Partial<ChatItem> & { toolName: string; toolKind: ChatItem['toolKind'] },
): ChatItem => ({
  kind: 'tool',
  content: '',
  toolId,
  ...overrides,
});

const parent = (overrides: Partial<ChatItem> = {}): ChatItem => ({
  kind: 'tool',
  content: '',
  toolId: 'task1',
  toolName: 'Task',
  isSubagent: true,
  toolArgs: '{"description":"调研登录 bug"}',
  ...overrides,
});

describe('SubagentTaskCard', () => {
  afterEach(cleanup);

  it('renders a bordered card container like tool cards', () => {
    const { container } = render(<SubagentTaskCard item={parent()} />);
    const root = container.firstChild as HTMLElement;
    // 问题③：subagent 父卡与 MessageBubble 工具卡同构（圆角细线边框 + muted 淡底）
    expect(root.className).toContain('border');
    expect(root.className).toContain('rounded-lg');
    expect(root.className).toContain('bg-muted/30');
  });

  it('shows running progress as "N 个工具 · 当前工具名" (no done/total)', () => {
    render(
      <SubagentTaskCard
        item={parent({
          children: [
            childTool('c1', { toolName: 'Read x', toolKind: 'read', toolStatus: 'completed', toolResult: 'ok' }),
            childTool('c2', { toolName: 'Bash', toolKind: 'execute', toolStatus: 'running' }),
          ],
        })}
      />,
    );
    // 当前运行工具 = 最后一个未完成的子工具卡 → Bash 归一化 label Terminal
    expect(screen.getByText('2 个工具 · Terminal')).toBeTruthy();
    // 不再显示 done/total（如 1/2）
    expect(screen.queryByText(/\/2/)).toBeNull();
    // 运行中：状态徽章不是 ✓
    expect(screen.queryByText('✓')).toBeNull();
  });

  it('shows plain "N 个工具" when completed', () => {
    render(
      <SubagentTaskCard
        item={parent({
          toolStatus: 'completed',
          toolResult: '调研完成',
          children: [
            childTool('c1', { toolName: 'Read x', toolKind: 'read', toolStatus: 'completed', toolResult: 'ok' }),
            childTool('c2', { toolName: 'Bash', toolKind: 'execute', toolStatus: 'completed', toolResult: 'done' }),
          ],
        })}
      />,
    );
    expect(screen.getByText('2 个工具')).toBeTruthy();
    expect(screen.getByText('✓')).toBeTruthy();
  });

  it('hides the progress segment when there are no child tools', () => {
    render(
      <SubagentTaskCard
        item={parent({ children: [{ kind: 'assistant', content: '子文本', parentToolId: 'task1' }] })}
      />,
    );
    expect(screen.queryByText(/个工具/)).toBeNull();
  });

  it('shows the breathing progress bar while running and fades out when done', () => {
    const { container, rerender } = render(
      <SubagentTaskCard
        item={parent({
          children: [childTool('c1', { toolName: 'Read x', toolKind: 'read', toolStatus: 'running' })],
        })}
      />,
    );
    const bar = container.querySelector('.h-0\\.5');
    expect(bar?.className).toContain('bg-muted');
    expect(container.querySelector('.animate-pulse')).toBeTruthy();
    rerender(
      <SubagentTaskCard
        item={parent({
          toolStatus: 'completed',
          toolResult: 'done',
          children: [
            childTool('c1', { toolName: 'Read x', toolKind: 'read', toolStatus: 'completed', toolResult: 'ok' }),
          ],
        })}
      />,
    );
    const barAfter = container.querySelector('.h-0\\.5');
    // 容器常驻（避免高度跳变），背景淡出为透明，动画条移除
    expect(barAfter?.className).toContain('bg-transparent');
    expect(container.querySelector('.animate-pulse')).toBeNull();
  });
});
