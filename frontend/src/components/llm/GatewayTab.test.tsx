// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import GatewayTab from './GatewayTab';
import type { LlmGatewayConfig } from '../../types';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string, p?: Record<string, unknown>) =>
    p && 'error' in p ? `${k}:${p['error']}` : k }),
}));

const captured = vi.hoisted(() => ({ update: vi.fn() }));

const baseConfig: LlmGatewayConfig = {
  enabled: true,
  openai_domain: 'o.example.com',
  anthropic_domain: null,
  listen: '0.0.0.0:443',
  tls_enabled: true,
  tls_acme: false,
};

vi.mock('@/api/hooks', async (orig) => {
  const actual = await orig<typeof import('@/api/hooks')>();
  return {
    ...actual,
    useLlmGatewayConfig: vi.fn(() => ({ data: baseConfig, isLoading: false })),
    useUpdateLlmGatewayConfig: vi.fn(() => ({
      mutate: captured.update,
      isPending: false,
    })),
  };
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const renderTab = () => render(<GatewayTab />);

describe('GatewayTab', () => {
  it('未编辑时保存按钮禁用（脏检查）', () => {
    renderTab();
    const save = screen.getByText('common.save').closest('button')!;
    expect(save.disabled).toBe(true);
  });

  it('修改输入后保存按钮启用并携带表单值', () => {
    renderTab();
    fireEvent.change(screen.getByPlaceholderText('anthropic.example.com'), {
      target: { value: 'a.example.com' },
    });
    const save = screen.getByText('common.save').closest('button')!;
    expect(save.disabled).toBe(false);

    fireEvent.click(save);
    expect(captured.update).toHaveBeenCalledWith(
      expect.objectContaining({ anthropic_domain: 'a.example.com' }),
      expect.objectContaining({ onError: expect.any(Function) }),
    );
  });

  it('保存失败时显示错误横幅', async () => {
    // 捕获 onError 并以错误对象调用，模拟 mutation 失败回调
    let onErrorCb: ((e: unknown) => void) | undefined;
    captured.update.mockImplementation((_v, opts) => {
      onErrorCb = opts.onError;
    });
    renderTab();
    fireEvent.change(screen.getByPlaceholderText('openai.example.com'), {
      target: { value: 'new.example.com' },
    });
    fireEvent.click(screen.getByText('common.save'));
    expect(onErrorCb).toBeTruthy();
    await act(async () => {
      onErrorCb!({ response: { data: 'boom' } });
    });

    expect(screen.getByText(/llm\.gateway\.saveError.*boom/s)).toBeTruthy();
  });
});
