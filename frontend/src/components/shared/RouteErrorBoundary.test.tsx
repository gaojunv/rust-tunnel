// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { RouteErrorBoundary } from './RouteErrorBoundary';

/** 按开关抛错的测试子组件。 */
function Bomb({ shouldThrow }: { shouldThrow: boolean }) {
  if (shouldThrow) throw new Error('boom');
  return <div>child ok</div>;
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

beforeEach(() => {
  // React 会把边界捕获的错误打到 console.error，用例里静默掉保持输出干净
  vi.spyOn(console, 'error').mockImplementation(() => {});
});

describe('RouteErrorBoundary', () => {
  it('正常渲染子组件', () => {
    render(
      <RouteErrorBoundary resetKey="/a" onGoHome={() => {}}>
        <Bomb shouldThrow={false} />
      </RouteErrorBoundary>,
    );
    expect(screen.getByText('child ok')).toBeTruthy();
  });

  it('子组件崩溃时渲染兜底面板（role=alert + 重试/回首页）', () => {
    const onGoHome = vi.fn();
    render(
      <RouteErrorBoundary resetKey="/a" onGoHome={onGoHome}>
        <Bomb shouldThrow />
      </RouteErrorBoundary>,
    );
    expect(screen.getByRole('alert')).toBeTruthy();
    expect(screen.getByText('Something went wrong')).toBeTruthy();
    expect(screen.getByText('boom')).toBeTruthy();
    expect(screen.queryByText('child ok')).toBeNull();

    fireEvent.click(screen.getByText('Go home'));
    expect(onGoHome).toHaveBeenCalledTimes(1);
  });

  it('点击重试重置边界并重新渲染子组件', () => {
    const { rerender } = render(
      <RouteErrorBoundary resetKey="/a" onGoHome={() => {}}>
        <Bomb shouldThrow />
      </RouteErrorBoundary>,
    );
    expect(screen.getByRole('alert')).toBeTruthy();

    // 重置瞬间子组件仍抛错会再次进入兜底；先换成不抛错的子组件再点重试
    rerender(
      <RouteErrorBoundary resetKey="/a" onGoHome={() => {}}>
        <Bomb shouldThrow={false} />
      </RouteErrorBoundary>,
    );
    fireEvent.click(screen.getByText('Retry'));
    expect(screen.getByText('child ok')).toBeTruthy();
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('resetKey 变化（导航）时自动重置错误态', () => {
    const { rerender } = render(
      <RouteErrorBoundary resetKey="/a" onGoHome={() => {}}>
        <Bomb shouldThrow />
      </RouteErrorBoundary>,
    );
    expect(screen.getByRole('alert')).toBeTruthy();

    rerender(
      <RouteErrorBoundary resetKey="/b" onGoHome={() => {}}>
        <Bomb shouldThrow={false} />
      </RouteErrorBoundary>,
    );
    expect(screen.getByText('child ok')).toBeTruthy();
    expect(screen.queryByRole('alert')).toBeNull();
  });
});
