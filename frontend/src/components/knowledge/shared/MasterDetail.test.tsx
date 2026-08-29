// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import MasterDetail from './MasterDetail';

afterEach(cleanup);

describe('MasterDetail', () => {
  it('loading 时渲染骨架（3 行 Skeleton）而非文本卡', () => {
    const { container } = render(
      <MasterDetail list={<div>list</div>} detail={<div>detail</div>} hasSelection={false} emptyText="空" isLoading loadingText="加载中" />,
    );
    const skeletons = container.querySelectorAll('.animate-pulse');
    expect(skeletons.length).toBe(3);
    expect(screen.queryByText('list')).toBeNull();
  });

  it('空态时展示 emptyText', () => {
    render(
      <MasterDetail
        list={<div>list</div>}
        detail={<div>detail</div>}
        hasSelection={false}
        emptyText="请选择一条"
        isLoading={false}
        loadingText="加载中"
      />,
    );
    expect(screen.getByText('请选择一条')).toBeTruthy();
    expect(screen.queryByText('detail')).toBeNull();
  });

  it('选中态展示 detail', () => {
    render(
      <MasterDetail list={<div>list</div>} detail={<div>详情内容</div>} hasSelection emptyText="空" isLoading={false} loadingText="加载中" />,
    );
    expect(screen.getByText('详情内容')).toBeTruthy();
  });
});
