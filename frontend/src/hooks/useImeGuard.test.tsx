// @vitest-environment jsdom
import { describe, expect, it, afterEach } from 'vitest';
import { cleanup, render, screen, act, fireEvent } from '@testing-library/react';
import { useState } from 'react';
import { useImeGuard } from './useImeGuard';

/** 测试宿主：回车提交计数器 —— 守卫生效则不计数。 */
function Host() {
  const ime = useImeGuard();
  const [submits, setSubmits] = useState(0);
  return (
    <>
      <input
        aria-label="field"
        {...ime.bind}
        onKeyDown={(e) => {
          if (ime.isComposing(e)) return;
          if (e.key === 'Enter') setSubmits((n) => n + 1);
        }}
      />
      <span data-testid="count">{submits}</span>
    </>
  );
}

const count = () => Number(screen.getByTestId('count').textContent);

describe('useImeGuard', () => {
  afterEach(cleanup);

  it('组词中回车不提交（isComposing=true，Chrome/多数输入法）', () => {
    render(<Host />);
    const el = screen.getByLabelText('field');
    fireEvent.compositionStart(el);
    fireEvent.keyDown(el, { key: 'Enter', isComposing: true });
    expect(count()).toBe(0);
  });

  it('keyCode=229 的确认键不提交（部分输入法不发 composition 事件）', () => {
    render(<Host />);
    const el = screen.getByLabelText('field');
    fireEvent.keyDown(el, { key: 'Enter', keyCode: 229, isComposing: false });
    expect(count()).toBe(0);
  });

  it('compositionend 先于确认回车时不提交（Safari 事件顺序）', () => {
    render(<Host />);
    const el = screen.getByLabelText('field');
    fireEvent.compositionStart(el);
    fireEvent.compositionEnd(el);
    // 该 keydown 的 isComposing=false、keyCode=13 → 仅靠延迟重置的 composing 标记兜住
    fireEvent.keyDown(el, { key: 'Enter', isComposing: false, keyCode: 13 });
    expect(count()).toBe(0);
  });

  it('组词结束（延迟重置生效）后回车正常提交', async () => {
    render(<Host />);
    const el = screen.getByLabelText('field');
    fireEvent.compositionStart(el);
    fireEvent.compositionEnd(el);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    fireEvent.keyDown(el, { key: 'Enter' });
    expect(count()).toBe(1);
  });

  it('从未组词时回车直接提交（英文输入不受影响）', () => {
    render(<Host />);
    const el = screen.getByLabelText('field');
    fireEvent.keyDown(el, { key: 'Enter' });
    expect(count()).toBe(1);
  });
});
