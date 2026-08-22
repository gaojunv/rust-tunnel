// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { ConfirmDialog, useConfirm } from './confirm-dialog';

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const Harness = ({ onAction }: { onAction: () => void }) => {
  const { open, payload, confirm, cancel, confirmAndClose } = useConfirm();
  return (
    <div>
      <button onClick={() => confirm({ title: 'T', description: 'D' }, onAction)}>arm</button>
      <ConfirmDialog
        open={open}
        payload={payload}
        onConfirm={confirmAndClose}
        onCancel={cancel}
        confirmLabel="OK"
        cancelLabel="Nope"
      />
    </div>
  );
};

describe('useConfirm + ConfirmDialog', () => {
  it('确认后执行动作并关闭弹窗', () => {
    const onAction = vi.fn();
    render(<Harness onAction={onAction} />);

    // 未触发前无对话框文案
    expect(screen.queryByText('T')).toBeNull();

    fireEvent.click(screen.getByText('arm'));
    expect(screen.getByText('T')).toBeTruthy();
    expect(screen.getByText('D')).toBeTruthy();

    fireEvent.click(screen.getByText('OK'));
    expect(onAction).toHaveBeenCalledTimes(1);
    // 确认后关闭
    expect(screen.queryByText('T')).toBeNull();
  });

  it('取消不执行动作并关闭弹窗', () => {
    const onAction = vi.fn();
    render(<Harness onAction={onAction} />);

    fireEvent.click(screen.getByText('arm'));
    fireEvent.click(screen.getByText('Nope'));
    expect(onAction).not.toHaveBeenCalled();
    expect(screen.queryByText('T')).toBeNull();
  });

  it('payload 为 null 时即使 open=true 也渲染空', () => {
    const NoPayload = () => (
      <ConfirmDialog open payload={null} onConfirm={vi.fn()} onCancel={vi.fn()} />
    );
    const { container } = render(<NoPayload />);
    expect(container.querySelector('[role="dialog"]')).toBeNull();
  });
});
