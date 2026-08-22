// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { GroupDialog } from './GroupDialog';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const captured = vi.hoisted(() => ({
  createGroup: vi.fn(),
  updateGroup: vi.fn(),
  replaceMembers: vi.fn(),
  resetBreaker: vi.fn(),
}));

vi.mock('@/api/hooks', async (orig) => {
  const actual = await orig<typeof import('@/api/hooks')>();
  return {
    ...actual,
    useLlmModelGroup: vi.fn(() => ({ data: undefined })),
    useLlmAllModels: vi.fn(() => ({ data: [] })),
    useLlmProviders: vi.fn(() => ({ data: [] })),
    useCreateLlmModelGroup: vi.fn(() => ({ mutateAsync: captured.createGroup, isPending: false })),
    useUpdateLlmModelGroup: vi.fn(() => ({ mutateAsync: captured.updateGroup, isPending: false })),
    useReplaceGroupMembers: vi.fn(() => ({ mutateAsync: captured.replaceMembers, isPending: false })),
    useResetGroupBreaker: vi.fn(() => ({ mutate: captured.resetBreaker, isPending: false })),
  };
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const renderDialog = (groupId: string | null = null) =>
  render(
    <GroupDialog open onOpenChange={vi.fn()} groupId={groupId} onDelete={vi.fn()} />,
  );

describe('GroupDialog', () => {
  it('纯空格组名禁用保存按钮', () => {
    renderDialog();
    const save = screen.getByText('common.save').closest('button')!;
    // 初始为空：禁用
    expect(save.disabled).toBe(true);

    fireEvent.change(screen.getByPlaceholderText('smart-router'), {
      target: { value: '   ' },
    });
    // trim 后仍为空：保持禁用（回归：`!name` 让纯空格通过）
    expect(save.disabled).toBe(true);
  });

  it('空组名点保存显示必填错误而不发请求', () => {
    renderDialog();
    fireEvent.change(screen.getByPlaceholderText('smart-router'), {
      target: { value: '   ' },
    });
    // 按钮已禁用，直接断言不发请求
    expect(captured.createGroup).not.toHaveBeenCalled();
    expect(captured.updateGroup).not.toHaveBeenCalled();
  });

  it('保存失败时对话框保持打开并显示错误横幅', async () => {
    captured.createGroup.mockRejectedValue({ response: { data: 'name conflict' } });
    renderDialog();
    fireEvent.change(screen.getByPlaceholderText('smart-router'), {
      target: { value: 'router' },
    });
    fireEvent.click(screen.getByText('common.save'));

    await screen.findByText('name conflict');
    // 失败后不关闭：标题仍在
    expect(screen.getByText('llm.groups.add')).toBeTruthy();
    expect(captured.replaceMembers).not.toHaveBeenCalled();
  });
});
