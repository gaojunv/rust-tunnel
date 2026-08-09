// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import ApprovalCard from './ApprovalCard';
import type { ChatItem } from './types';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

afterEach(cleanup);

const baseItem: ChatItem = {
  kind: 'approval',
  content: '',
  approvalId: 'req1',
  approvalTool: 'shell',
  approvalSummary: 'rm -rf /tmp/x',
  approvalStatus: 'pending',
};

describe('ApprovalCard', () => {
  it('renders approve/deny binary buttons when no options', () => {
    render(<ApprovalCard item={baseItem} onRespond={vi.fn()} />);
    expect(screen.getByRole('button', { name: 'agent.approveOnce' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'agent.approveSession' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'agent.deny' })).toBeTruthy();
  });

  it('renders ACP permission options and returns option_id on click', () => {
    const onRespond = vi.fn();
    const item: ChatItem = {
      ...baseItem,
      approvalOptions: [
        { id: 'allow_once', label: '允许一次', kind: 'allow_once' },
        { id: 'allow_always', label: '总是允许', kind: 'allow_always' },
        { id: 'reject', label: '拒绝', kind: 'reject_once' },
      ],
    };
    render(<ApprovalCard item={item} onRespond={onRespond} />);
    // 选项按钮渲染，二元按钮不出现
    expect(screen.getByRole('button', { name: /允许一次/ })).toBeTruthy();
    expect(screen.getByRole('button', { name: /总是允许/ })).toBeTruthy();
    expect(screen.getByRole('button', { name: /拒绝/ })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'agent.approveOnce' })).toBeNull();

    // allow_always → approved=true, remember=true, 原样回传 option_id
    fireEvent.click(screen.getByRole('button', { name: /总是允许/ }));
    expect(onRespond).toHaveBeenCalledWith('req1', true, true, 'allow_always');

    // reject_once → approved=false, remember=false
    fireEvent.click(screen.getByRole('button', { name: /拒绝/ }));
    expect(onRespond).toHaveBeenCalledWith('req1', false, false, 'reject');

    // allow_once → approved=true, remember=false
    fireEvent.click(screen.getByRole('button', { name: /允许一次/ }));
    expect(onRespond).toHaveBeenCalledWith('req1', true, false, 'allow_once');
  });

  it('renders resolved status text when not pending', () => {
    const item: ChatItem = { ...baseItem, approvalStatus: 'approved' };
    render(<ApprovalCard item={item} onRespond={vi.fn()} />);
    expect(screen.getByText(/agent.approved/)).toBeTruthy();
    expect(screen.queryByRole('button')).toBeNull();
  });
});
