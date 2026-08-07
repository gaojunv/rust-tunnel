// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import TerminalPanel from './TerminalPanel';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

// xterm 在 jsdom 下需要 canvas/DOM 测量，完整渲染不现实 —— mock 掉两个模块。
// vi.hoisted 保证 mock 工厂与测试体共享同一组实例引用。
const h = vi.hoisted(() => {
  class FakeTerminal {
    cols = 80;
    rows = 24;
    options: Record<string, unknown> = {};
    open = vi.fn();
    write = vi.fn();
    focus = vi.fn();
    dispose = vi.fn();
    loadAddon = vi.fn();
    onDataCb: ((d: string) => void) | null = null;
    onData = vi.fn((cb: (d: string) => void) => {
      this.onDataCb = cb;
      return { dispose: vi.fn() };
    });
    constructor(options: Record<string, unknown>) {
      this.options = options;
      terminals.push(this);
    }
    emitData(d: string) {
      this.onDataCb?.(d);
    }
  }
  class FakeFitAddon {
    fit = vi.fn();
  }
  const terminals: FakeTerminal[] = [];
  return { FakeTerminal, FakeFitAddon, terminals };
});

vi.mock('@xterm/xterm', () => ({
  Terminal: h.FakeTerminal,
}));

vi.mock('@xterm/addon-fit', () => ({
  FitAddon: h.FakeFitAddon,
}));

// 捕获 ws 实例以便手动触发 onopen/onmessage/onclose（参照 ChatStream.test 的 FakeWs 模式）
let wsInstance: FakeWs | null = null;
const wsInstances: FakeWs[] = [];

class FakeWs {
  static OPEN = 1;
  readyState = 1;
  binaryType = '';
  url = '';
  sent: (string | ArrayBuffer | Uint8Array)[] = [];
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onopen: (() => void) | null = null;
  constructor(url: string) {
    this.url = url;
    // eslint-disable-next-line @typescript-eslint/no-this-alias -- 捕获实例以便手动触发 onopen/onmessage/onclose
    wsInstance = this;
    wsInstances.push(this);
  }
  send(d: string | ArrayBuffer | Uint8Array) {
    this.sent.push(d);
  }
  close() {}
}

// jsdom 没有 ResizeObserver：stub 一个空实现（组件内已用 typeof 守卫兜底）
class FakeResizeObserver {
  observe() {}
  disconnect() {}
}

describe('TerminalPanel', () => {
  beforeEach(() => {
    vi.stubGlobal('WebSocket', FakeWs as unknown as typeof WebSocket);
    vi.stubGlobal('ResizeObserver', FakeResizeObserver);
    wsInstance = null;
    wsInstances.length = 0;
    h.terminals.length = 0;
  });

  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('creates a terminal and opens a WebSocket carrying workspace_id/cols/rows/token', () => {
    render(<TerminalPanel workspaceId="w1" />);
    // xterm 构造 + FitAddon 装载 + open + fit
    expect(h.terminals).toHaveLength(1);
    expect(h.terminals[0].loadAddon).toHaveBeenCalledTimes(1);
    expect(h.terminals[0].open).toHaveBeenCalledTimes(1);
    expect(h.terminals[0].options).toMatchObject({ fontSize: 12, cursorBlink: true });
    // 初始化时 jsdom 无 dark class → 浅色主题
    expect(h.terminals[0].options.theme).toEqual({
      background: '#ffffff',
      foreground: '#1e293b',
    });
    // WebSocket 构造且 URL 携带协商尺寸与 token
    expect(wsInstances).toHaveLength(1);
    const url = wsInstances[0].url;
    expect(url).toContain('/api/agent/terminal/ws');
    expect(url).toContain('workspace_id=w1');
    expect(url).toContain('cols=80');
    expect(url).toContain('rows=24');
    expect(url).toContain('token=');
    // 初始状态：连接中
    expect(screen.getByText('agent.terminalConnecting')).toBeTruthy();
  });

  it('forwards terminal input to the WebSocket as binary (not Text frame)', () => {
    // 协议约定双向仅用 Binary 帧：后端 bridge_terminal 只消费 Message::Binary，
    // Text 帧被静默忽略——输入必须编码为字节而非字符串（回归：曾用 ws.send(string)
    // 发送 Text 帧导致按键全部丢失）。
    render(<TerminalPanel workspaceId="w1" />);
    act(() => {
      h.terminals[0].emitData('ls');
    });
    expect(wsInstances[0].sent).toHaveLength(1);
    const sent = wsInstances[0].sent[0];
    // 必须是二进制帧而非字符串（Text 帧）。jsdom 与 Node 的 Uint8Array 属不同
    // realm，跨 realm 的 instanceof 会失败——用 ArrayBuffer.isView + 内容断言。
    expect(sent).not.toBeTypeOf('string');
    expect(ArrayBuffer.isView(sent)).toBe(true);
    expect(new TextDecoder().decode(sent as Uint8Array)).toBe('ls');
  });

  it('writes binary frames from the server into the terminal', () => {
    render(<TerminalPanel workspaceId="w1" />);
    act(() => {
      wsInstance!.onmessage?.({ data: new ArrayBuffer(4) });
    });
    expect(h.terminals[0].write).toHaveBeenCalledWith(expect.any(Uint8Array));
  });

  it('renders a text error frame from the server in the terminal', () => {
    render(<TerminalPanel workspaceId="w1" />);
    act(() => {
      wsInstance!.onmessage?.({ data: 'client does not support pty' });
    });
    expect(h.terminals[0].write).toHaveBeenCalledWith(
      expect.stringContaining('client does not support pty'),
    );
  });

  it('tracks status through open/close and reconnects with a fresh WebSocket', () => {
    render(<TerminalPanel workspaceId="w1" />);
    // open → 已连接，无重连按钮
    act(() => {
      wsInstance!.onopen?.();
    });
    expect(screen.getByText('agent.terminalConnected')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'agent.terminalReconnect' })).toBeNull();
    // close → 已断开 + 显示重连按钮
    act(() => {
      wsInstance!.onclose?.();
    });
    expect(screen.getByText('agent.terminalDisconnected')).toBeTruthy();
    // onclose 在终端内写入提示（mock 的 write 上断言，DOM 里没有该文本）
    expect(h.terminals[0].write).toHaveBeenCalledWith(expect.stringContaining('[connection closed]'));
    const reconnect = screen.getByRole('button', { name: 'agent.terminalReconnect' });
    // 点击重连 → 旧终端/旧连接被清理，重建终端 + 新 WebSocket
    fireEvent.click(reconnect);
    expect(wsInstances).toHaveLength(2);
    expect(h.terminals).toHaveLength(2);
    expect(h.terminals[0].dispose).toHaveBeenCalledTimes(1);
    // 新连接回到连接中状态
    expect(screen.getByText('agent.terminalConnecting')).toBeTruthy();
  });

  it('does not mount a terminal when workspaceId is empty', () => {
    render(<TerminalPanel workspaceId="" />);
    expect(h.terminals).toHaveLength(0);
    expect(wsInstances).toHaveLength(0);
    expect(screen.getByText('agent.terminalDisconnected')).toBeTruthy();
  });
});
