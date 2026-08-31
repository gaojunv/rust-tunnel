// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import SectionFrame from './SectionFrame';

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe('SectionFrame', () => {
  it('渲染设置按钮文字并触发回调', () => {
    const onSettings = vi.fn();
    const onNew = vi.fn();
    render(
      <SectionFrame title="知识库" count={3} newLabel="新建" onNew={onNew} onSettings={onSettings} settingsLabel="设置">
        <div>child</div>
      </SectionFrame>,
    );
    const btn = screen.getByRole('button', { name: '设置' });
    expect(btn.textContent).toContain('设置');
    fireEvent.click(btn);
    expect(onSettings).toHaveBeenCalledTimes(1);
  });

  it('无 onSettings 时不渲染设置按钮', () => {
    render(
      <SectionFrame title="T" count={0} newLabel="新建" onNew={vi.fn()} settingsLabel="设置">
        <div>child</div>
      </SectionFrame>,
    );
    expect(screen.queryByRole('button', { name: '设置' })).toBeNull();
  });

  it('新建按钮点击触发 onNew', () => {
    const onNew = vi.fn();
    render(
      <SectionFrame title="T" count={0} newLabel="新建容器" onNew={onNew} settingsLabel="设置">
        <div>child</div>
      </SectionFrame>,
    );
    fireEvent.click(screen.getByRole('button', { name: '新建容器' }));
    expect(onNew).toHaveBeenCalledTimes(1);
  });
});
