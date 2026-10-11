// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import RustdeskConfigCard from './RustdeskConfigCard';
import type { RustdeskConfig } from '../../types';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({
    t: (k: string, p?: Record<string, unknown>) => {
      if (!p) return k;
      if ('port' in p) return `${k}:${String(p['port'])}`;
      if ('count' in p) return `${k}:${String(p['count'])}`;
      return k;
    },
  }),
}));

const mocks = vi.hoisted(() => ({
  update: vi.fn(),
  config: vi.fn(),
}));

vi.mock('@/api/hooks', async (orig) => {
  const actual = await orig<typeof import('@/api/hooks')>();
  return {
    ...actual,
    useRustdeskConfig: () => mocks.config(),
    useUpdateRustdeskConfig: () => ({ mutate: mocks.update, isPending: false }),
  };
});

const baseConfig: RustdeskConfig = {
  enabled: true,
  rendezvous_port: 21116,
  relay_port: 21117,
  relay_server: 'relay.example.com:21117',
  always_use_relay: false,
  local_ip: '192.168.1.10',
  mask: '192.168.0.0/16',
  key_set: true,
  running: true,
  nat_test_port: 21115,
  online_peers: 3,
  known_peers: 5,
  relay_pairs: 1,
};

/** 已加载且未出错的 GET 状态。 */
const loaded = () => ({
  data: baseConfig,
  isLoading: false,
  isError: false,
  refetch: vi.fn(),
});

beforeEach(() => {
  mocks.update.mockReset();
  mocks.config.mockReset();
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe('RustdeskConfigCard', () => {
  it('加载中渲染骨架屏而非表单', () => {
    mocks.config.mockReturnValue({
      data: undefined,
      isLoading: true,
      isError: false,
      refetch: vi.fn(),
    });
    const { container } = render(<RustdeskConfigCard />);

    expect(container.querySelectorAll('.animate-pulse').length).toBeGreaterThan(0);
    expect(screen.queryByPlaceholderText('21116')).toBeNull();
  });

  it('加载失败渲染 ErrorState，点击重试触发 refetch', () => {
    const refetch = vi.fn();
    mocks.config.mockReturnValue({
      data: undefined,
      isLoading: false,
      isError: true,
      refetch,
    });
    render(<RustdeskConfigCard />);

    const retry = screen.getByText('common.retry');
    expect(retry).toBeTruthy();
    fireEvent.click(retry);
    expect(refetch).toHaveBeenCalledTimes(1);
  });

  it('保存时携带表单值，且空 key 不进载荷', () => {
    mocks.config.mockReturnValue(loaded());
    render(<RustdeskConfigCard />);

    fireEvent.change(screen.getByPlaceholderText('21116'), { target: { value: '30000' } });
    fireEvent.click(screen.getByText('rustdesk.config.save'));

    expect(mocks.update).toHaveBeenCalledTimes(1);
    const payload = mocks.update.mock.calls[0]?.[0] as Record<string, unknown>;
    expect(payload).toEqual({
      enabled: true,
      rendezvous_port: 30000,
      relay_port: 21117,
      relay_server: 'relay.example.com:21117',
      always_use_relay: false,
      local_ip: '192.168.1.10',
      mask: '192.168.0.0/16',
    });
    expect('key' in payload).toBe(false);
  });

  it('生成密钥以 generate_key=true 提交并一次性展示返回的密钥', async () => {
    const captured: {
      payload?: Record<string, unknown>;
      opts?: { onSuccess?: (d: { key?: string }) => void };
    } = {};
    mocks.update.mockImplementation((payload, opts) => {
      captured.payload = payload;
      captured.opts = opts;
    });
    mocks.config.mockReturnValue(loaded());
    render(<RustdeskConfigCard />);

    await act(async () => {
      fireEvent.click(screen.getByText('rustdesk.config.generateKey'));
    });

    expect(captured.payload).toMatchObject({ generate_key: true, enabled: true });
    expect(screen.queryByText('RDKEY-abc123')).toBeNull();

    await act(async () => {
      captured.opts?.onSuccess?.({ key: 'RDKEY-abc123' });
    });

    expect(screen.getByText('RDKEY-abc123')).toBeTruthy();
    expect(screen.getByText('rustdesk.config.generatedKey.hint')).toBeTruthy();
  });

  it('会合端口非法（空/越界）时禁用保存且不发请求', () => {
    mocks.config.mockReturnValue(loaded());
    render(<RustdeskConfigCard />);

    const save = screen.getByRole('button', { name: 'rustdesk.config.save' });

    // 空串
    fireEvent.change(screen.getByPlaceholderText('21116'), { target: { value: '' } });
    expect(screen.getByText('rustdesk.config.errors.rendezvousPort')).toBeTruthy();
    expect((save as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(save);

    // 下界越界（会合端口最小 2）
    fireEvent.change(screen.getByPlaceholderText('21116'), { target: { value: '1' } });
    expect((save as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(save);

    // 上界越界（> 65535）
    fireEvent.change(screen.getByPlaceholderText('21116'), { target: { value: '70000' } });
    expect((save as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(save);

    expect(mocks.update).not.toHaveBeenCalled();

    // 恢复合法值后可保存
    fireEvent.change(screen.getByPlaceholderText('21116'), { target: { value: '21116' } });
    expect((save as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(save);
    expect(mocks.update).toHaveBeenCalledTimes(1);
  });

  it('中继端口与会合端口（或 NAT 测试端口）冲突时禁用保存', () => {
    mocks.config.mockReturnValue(loaded());
    render(<RustdeskConfigCard />);

    const save = screen.getByRole('button', { name: 'rustdesk.config.save' });

    // 21116 = 会合端口
    fireEvent.change(screen.getByPlaceholderText('21117'), { target: { value: '21116' } });
    expect(screen.getByText('rustdesk.config.errors.relayPortConflict')).toBeTruthy();
    expect((save as HTMLButtonElement).disabled).toBe(true);

    // 21115 = 会合端口 - 1（NAT 测试端口）
    fireEvent.change(screen.getByPlaceholderText('21117'), { target: { value: '21115' } });
    expect(screen.getByText('rustdesk.config.errors.relayPortConflict')).toBeTruthy();

    // 空串 = 非法整数
    fireEvent.change(screen.getByPlaceholderText('21117'), { target: { value: '' } });
    expect(screen.getByText('rustdesk.config.errors.relayPort')).toBeTruthy();

    fireEvent.click(save);
    expect(mocks.update).not.toHaveBeenCalled();
  });

  it('relay_server / mask 非法时禁用保存（空串则合法）', () => {
    mocks.config.mockReturnValue(loaded());
    render(<RustdeskConfigCard />);

    const save = screen.getByRole('button', { name: 'rustdesk.config.save' });

    // 缺端口
    fireEvent.change(screen.getByPlaceholderText('relay.example.com:21117'), {
      target: { value: 'relay.example.com' },
    });
    expect(screen.getByText('rustdesk.config.errors.relayServer')).toBeTruthy();
    expect((save as HTMLButtonElement).disabled).toBe(true);

    // 端口为 0
    fireEvent.change(screen.getByPlaceholderText('relay.example.com:21117'), {
      target: { value: 'relay.example.com:0' },
    });
    expect(screen.getByText('rustdesk.config.errors.relayServer')).toBeTruthy();

    // 前缀越界
    fireEvent.change(screen.getByPlaceholderText('relay.example.com:21117'), {
      target: { value: '' },
    });
    fireEvent.change(screen.getByPlaceholderText('192.168.0.0/16'), {
      target: { value: '192.168.0.0/33' },
    });
    expect(screen.getByText('rustdesk.config.errors.mask')).toBeTruthy();
    expect((save as HTMLButtonElement).disabled).toBe(true);

    // 空串（合法：不下发）后恢复可保存
    fireEvent.change(screen.getByPlaceholderText('192.168.0.0/16'), { target: { value: '' } });
    expect((save as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(save);
    expect(mocks.update).toHaveBeenCalledTimes(1);
  });

  it('mask 八位组带前导零判为非法（与后端 Ipv4Addr 解析同源）', () => {
    mocks.config.mockReturnValue(loaded());
    render(<RustdeskConfigCard />);

    const save = screen.getByRole('button', { name: 'rustdesk.config.save' });
    const maskInput = screen.getByPlaceholderText('192.168.0.0/16');

    fireEvent.change(maskInput, { target: { value: '192.168.01.1' } });
    expect(screen.getByText('rustdesk.config.errors.mask')).toBeTruthy();
    expect((save as HTMLButtonElement).disabled).toBe(true);

    // 去掉前导零后合法
    fireEvent.change(maskInput, { target: { value: '192.168.1.0/16' } });
    expect(screen.queryByText('rustdesk.config.errors.mask')).toBeNull();
    expect((save as HTMLButtonElement).disabled).toBe(false);
  });

  it('mask 前缀允许前导零、上限仍是 32（与后端 u8::from_str 一致）', () => {
    mocks.config.mockReturnValue(loaded());
    render(<RustdeskConfigCard />);

    const save = screen.getByRole('button', { name: 'rustdesk.config.save' });
    const maskInput = screen.getByPlaceholderText('192.168.0.0/16');

    // /016、/032 等价于 /16、/32，后端 u8::from_str 接受 → 前端也放行
    fireEvent.change(maskInput, { target: { value: '192.168.0.0/016' } });
    expect(screen.queryByText('rustdesk.config.errors.mask')).toBeNull();
    fireEvent.change(maskInput, { target: { value: '192.168.0.0/032' } });
    expect(screen.queryByText('rustdesk.config.errors.mask')).toBeNull();
    expect((save as HTMLButtonElement).disabled).toBe(false);

    // /256、/033 超出 0–32 → 判非法（与后端 prefix > 32 一致）
    fireEvent.change(maskInput, { target: { value: '192.168.0.0/256' } });
    expect(screen.getByText('rustdesk.config.errors.mask')).toBeTruthy();
    fireEvent.change(maskInput, { target: { value: '192.168.0.0/033' } });
    expect(screen.getByText('rustdesk.config.errors.mask')).toBeTruthy();
    expect((save as HTMLButtonElement).disabled).toBe(true);
  });

  it('last_error 非空时渲染持久错误横幅，为空时不渲染', () => {
    mocks.config.mockReturnValue({
      ...loaded(),
      data: { ...baseConfig, last_error: 'bind 21116 failed: address in use' },
    });
    const { unmount } = render(<RustdeskConfigCard />);

    const alert = screen.getByRole('alert');
    expect(alert.textContent).toBe('rustdesk.config.lastError.message');
    unmount();

    // running=true 时服务端恒为 null → 无横幅
    mocks.config.mockReturnValue(loaded());
    render(<RustdeskConfigCard />);
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('生成密钥失败时清空上一次回显的密钥，且表单内容保留', async () => {
    const captured: {
      opts?: {
        onSuccess?: (d: { key?: string }) => void;
        onError?: (e: unknown) => void;
      };
    } = {};
    mocks.update.mockImplementation((_payload, opts) => {
      captured.opts = opts;
    });
    mocks.config.mockReturnValue(loaded());
    render(<RustdeskConfigCard />);

    // 第一次成功：展示密钥
    await act(async () => {
      fireEvent.click(screen.getByText('rustdesk.config.generateKey'));
    });
    await act(async () => {
      captured.opts?.onSuccess?.({ key: 'RDKEY-abc123' });
    });
    expect(screen.getByText('RDKEY-abc123')).toBeTruthy();

    // 第二次失败：旧的密钥必须被清掉
    await act(async () => {
      fireEvent.click(screen.getByText('rustdesk.config.generateKey'));
    });
    await act(async () => {
      captured.opts?.onError?.(new Error('boom'));
    });

    expect(screen.queryByText('RDKEY-abc123')).toBeNull();
    expect(screen.queryByText('rustdesk.config.generatedKey.title')).toBeNull();
    // 表单内容保留（用户可修好后重试）
    expect((screen.getByPlaceholderText('21116') as HTMLInputElement).value).toBe('21116');
  });

  it('非空 key 进入保存载荷', () => {
    mocks.config.mockReturnValue(loaded());
    render(<RustdeskConfigCard />);

    fireEvent.change(screen.getByPlaceholderText('••••••••'), { target: { value: 'MYKEY-123' } });
    fireEvent.click(screen.getByText('rustdesk.config.save'));

    expect(mocks.update).toHaveBeenCalledTimes(1);
    expect(mocks.update.mock.calls[0]?.[0]).toMatchObject({ key: 'MYKEY-123' });
  });
});
