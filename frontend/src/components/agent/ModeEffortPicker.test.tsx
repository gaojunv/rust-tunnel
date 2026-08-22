// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import ModeEffortPicker from './ModeEffortPicker';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

afterEach(cleanup);

const modeOption = {
  id: 'mode',
  name: 'Mode',
  category: 'mode',
  type: 'select' as const,
  currentValue: 'plan',
  options: [
    { value: 'default', name: 'Default', description: 'Ask before edits' },
    { value: 'plan', name: 'Plan' },
  ],
};

const effortOption = {
  id: 'effort',
  name: 'Effort',
  category: 'thought_level',
  type: 'select' as const,
  currentValue: 'medium',
  options: [
    { value: 'low', name: 'Low' },
    { value: 'medium', name: 'Medium' },
    { value: 'high', name: 'High' },
  ],
};

/** 打开面板（Radix 在 pointerdown 展开，与其它 agent 测试一致） */
const open = (name = 'agent.configMode') => {
  const trigger = screen.getByRole('button', { name });
  fireEvent.pointerDown(trigger);
  return trigger;
};

describe('ModeEffortPicker', () => {
  it('外部胶囊只显示当前 mode，不显示 effort', () => {
    render(
      <ModeEffortPicker modeOption={modeOption} effortOption={effortOption} onChange={vi.fn()} />,
    );
    const trigger = screen.getByRole('button', { name: 'agent.configMode' });
    expect(trigger.textContent).toContain('Plan');
    expect(trigger.textContent).not.toContain('Medium');
  });

  it('面板同时含 mode 取值列表与 effort 单行滑条', () => {
    render(
      <ModeEffortPicker modeOption={modeOption} effortOption={effortOption} onChange={vi.fn()} />,
    );
    open();
    expect(screen.getByRole('menu')).toBeTruthy();
    // mode 列表项（含 description 副行）
    expect(screen.getByRole('menuitem', { name: /Default/ })).toBeTruthy();
    expect(screen.getByText('Ask before edits')).toBeTruthy();
    // effort 行：内联标题 + 当前档名 + 滑条停在当前档
    expect(screen.getByText('agent.configEffort')).toBeTruthy();
    expect(screen.getByText('Medium')).toBeTruthy();
    const slider = screen.getByRole('slider') as HTMLInputElement;
    expect(slider.value).toBe('1');
    expect(slider.max).toBe('2');
  });

  it('点击 mode 取值发一次 onChange', () => {
    const onChange = vi.fn();
    render(
      <ModeEffortPicker modeOption={modeOption} effortOption={effortOption} onChange={onChange} />,
    );
    open();
    fireEvent.click(screen.getByRole('menuitem', { name: /Default/ }));
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenCalledWith('mode', 'default');
  });

  it('滑条拖动中不发帧，松手才提交一次', () => {
    const onChange = vi.fn();
    render(
      <ModeEffortPicker modeOption={modeOption} effortOption={effortOption} onChange={onChange} />,
    );
    open();
    const slider = screen.getByRole('slider');
    fireEvent.change(slider, { target: { value: '0' } });
    fireEvent.change(slider, { target: { value: '2' } });
    // 拖动过程只更新本地视觉（当前档名跟着变），不刷中间档给 agent
    expect(onChange).not.toHaveBeenCalled();
    expect(screen.getByText('High')).toBeTruthy();
    fireEvent.pointerUp(slider);
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenCalledWith('effort', 'high');
  });

  it('滑回当前档不发帧（幂等）', () => {
    const onChange = vi.fn();
    render(
      <ModeEffortPicker modeOption={modeOption} effortOption={effortOption} onChange={onChange} />,
    );
    open();
    const slider = screen.getByRole('slider');
    fireEvent.change(slider, { target: { value: '2' } });
    fireEvent.change(slider, { target: { value: '1' } });
    fireEvent.pointerUp(slider);
    expect(onChange).not.toHaveBeenCalled();
  });

  it('面板关闭时 flush 未提交的档位', () => {
    const onChange = vi.fn();
    render(
      <ModeEffortPicker modeOption={modeOption} effortOption={effortOption} onChange={onChange} />,
    );
    open();
    fireEvent.change(screen.getByRole('slider'), { target: { value: '0' } });
    // 漏掉 release 事件（指针移出 / Esc 关闭）也不丢用户选择
    fireEvent.keyDown(screen.getByRole('menu'), { key: 'Escape' });
    expect(onChange).toHaveBeenCalledWith('effort', 'low');
  });

  it('effort 行左右键即时调档', () => {
    const onChange = vi.fn();
    render(
      <ModeEffortPicker modeOption={modeOption} effortOption={effortOption} onChange={onChange} />,
    );
    open();
    const row = screen.getByRole('menuitem', { name: /agent.configEffort/ });
    fireEvent.keyDown(row, { key: 'ArrowRight' });
    expect(onChange).toHaveBeenCalledWith('effort', 'high');
  });

  it('mode 缺失时胶囊退化显示 effort 当前值', () => {
    render(
      <ModeEffortPicker modeOption={undefined} effortOption={effortOption} onChange={vi.fn()} />,
    );
    const trigger = screen.getByRole('button', { name: 'agent.configEffort' });
    expect(trigger.textContent).toContain('Medium');
    expect(screen.queryByRole('button', { name: 'agent.configMode' })).toBeNull();
  });

  it('effort 缺失时面板内提示模型不支持且无滑条', () => {
    render(<ModeEffortPicker modeOption={modeOption} effortOption={undefined} onChange={vi.fn()} />);
    open();
    expect(screen.getByText('agent.configOptionUnsupported')).toBeTruthy();
    expect(screen.queryByRole('slider')).toBeNull();
  });

  it('两项都缺：placeholder 渲染禁用占位，否则整体隐藏', () => {
    const { container, unmount } = render(
      <ModeEffortPicker modeOption={undefined} effortOption={undefined} onChange={vi.fn()} />,
    );
    expect(container.firstChild).toBeNull();
    unmount();

    render(
      <ModeEffortPicker
        modeOption={undefined}
        effortOption={undefined}
        onChange={vi.fn()}
        placeholder
      />,
    );
    const btn = screen.getByRole('button', { name: 'agent.configMode' }) as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    expect(btn.getAttribute('title')).toBe('agent.configOptionUnsupported');
  });

  it('服务端值变化后覆盖本地拖动态（失败回滚可见）', () => {
    const { rerender } = render(
      <ModeEffortPicker modeOption={modeOption} effortOption={effortOption} onChange={vi.fn()} />,
    );
    open();
    fireEvent.change(screen.getByRole('slider'), { target: { value: '2' } });
    expect(screen.getByText('High')).toBeTruthy();
    // 服务端权威值改为 low（乐观值被回滚/agent 自行调整）
    rerender(
      <ModeEffortPicker
        modeOption={modeOption}
        effortOption={{ ...effortOption, currentValue: 'low' }}
        onChange={vi.fn()}
      />,
    );
    expect(screen.getByText('Low')).toBeTruthy();
    expect((screen.getByRole('slider') as HTMLInputElement).value).toBe('0');
  });
});
