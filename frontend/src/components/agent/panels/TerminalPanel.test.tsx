// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import TerminalPanel, { TERMINAL_THEME, handleTerminalKey } from './TerminalPanel';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

// xterm 在 jsdom 下需要 canvas/DOM 测量，完整渲染不现实 —— mock 掉两个模块。
// vi.hoisted 保证 mock 工厂与测试体共享同一组实例引用。
const h = vi.hoisted(() => {
  type FakeLine = { translateToString: (trim: boolean) => string };
  class FakeTerminal {
    cols = 80;
    rows = 24;
    options: Record<string, unknown> = {};
    // buffer.active 模拟滚动缓冲，用于无选区复制分支
    buffer: { active: { length: number; getLine: (i: number) => FakeLine | undefined } } = {
      active: { length: 0, getLine: () => undefined },
    };
    // 由测试预置的缓冲行内容
    _bufferLines: string[] = [];
    open = vi.fn();
    write = vi.fn();
    focus = vi.fn();
    dispose = vi.fn();
    loadAddon = vi.fn();
    clear = vi.fn();
    hasSelection = vi.fn(() => false);
    getSelection = vi.fn(() => 'selected');
    attachCustomKeyEventHandler = vi.fn((cb: (e: KeyboardEvent) => boolean) => {
      this.keyHandler = cb;
      return true;
    });
    keyHandler: ((e: KeyboardEvent) => boolean) | null = null;
    onDataCb: ((d: string) => void) | null = null;
    onData = vi.fn((cb: (d: string) => void) => {
      this.onDataCb = cb;
      return { dispose: vi.fn() };
    });
    constructor(options: Record<string, unknown>) {
      this.options = { ...options };
      // 让 options.theme 可写（MutationObserver 回调会直接赋值）
      Object.defineProperty(this, 'options', {
        value: this.options,
        writable: true,
        configurable: true,
        enumerable: true,
      });
      // 同步 _bufferLines 到 buffer.active
      this._syncBuffer();
      terminals.push(this);
    }
    _syncBuffer() {
      this.buffer.active.length = this._bufferLines.length;
      this.buffer.active.getLine = (i: number) => {
        const line = this._bufferLines[i];
        if (line === undefined) return undefined;
        return { translateToString: () => line } as FakeLine;
      };
    }
    setBufferLines(lines: string[]) {
      this._bufferLines = lines;
      this._syncBuffer();
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
    document.documentElement.classList.remove('dark');
    // clipboard 默认 mock（每个用例可覆写）
    Object.defineProperty(navigator, 'clipboard', {
      value: { writeText: vi.fn().mockResolvedValue(undefined), readText: vi.fn().mockResolvedValue('pasted') },
      writable: true,
      configurable: true,
    });
    // execCommand 降级路径需要
    document.execCommand = vi.fn(() => true);
  });

  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it('creates a terminal and opens a WebSocket carrying workspace_id/cols/rows/token', () => {
    render(<TerminalPanel workspaceId="w1" />);
    // xterm 构造 + FitAddon 装载 + open + fit
    expect(h.terminals).toHaveLength(1);
    expect(h.terminals[0].loadAddon).toHaveBeenCalledTimes(1);
    expect(h.terminals[0].open).toHaveBeenCalledTimes(1);
    expect(h.terminals[0].options).toMatchObject({ fontSize: 12, cursorBlink: true, rightClickSelectsWord: true });
    // 初始化时 jsdom 无 dark class → 浅色完整主题（含 cursor/selection）
    expect(h.terminals[0].options.theme).toEqual(TERMINAL_THEME.light);
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

  it('renders copy/paste/clear toolbar and wires clear to term.clear', async () => {
    render(<TerminalPanel workspaceId="w1" />);
    expect(screen.getByRole('button', { name: 'agent.terminalCopy' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'agent.terminalPaste' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'agent.terminalClear' })).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'agent.terminalClear' }));
    expect(h.terminals[0].clear).toHaveBeenCalledTimes(1);
  });

  it('copy uses selection when present and falls back to buffer with trailing blank trimming', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', { value: { writeText, readText: vi.fn() }, writable: true, configurable: true });
    render(<TerminalPanel workspaceId="w1" />);
    const term = h.terminals[0];
    // 有选区 → 直接取 getSelection
    term.hasSelection.mockReturnValue(true);
    term.getSelection.mockReturnValue('hello world');
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: 'agent.terminalCopy' }));
      await Promise.resolve();
    });
    expect(writeText).toHaveBeenCalledWith('hello world');
    writeText.mockClear();
    // 无选区 → 遍历 buffer，尾部空行被裁掉，中间空行保留
    term.hasSelection.mockReturnValue(false);
    term.setBufferLines(['line1', '', 'line3', '   ', '']);
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: 'agent.terminalCopy' }));
      await Promise.resolve();
    });
    expect(writeText).toHaveBeenCalledWith('line1\n\nline3');
  });

  it('copy falls back to hidden textarea + execCommand when clipboard unavailable', async () => {
    Object.defineProperty(navigator, 'clipboard', { value: undefined, writable: true, configurable: true });
    const execSpy = vi.fn(() => true);
    document.execCommand = execSpy as unknown as typeof document.execCommand;
    render(<TerminalPanel workspaceId="w1" />);
    const term = h.terminals[0];
    term.hasSelection.mockReturnValue(true);
    term.getSelection.mockReturnValue('fallback text');
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: 'agent.terminalCopy' }));
      // 等待 async copy 完成（微任务）
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(execSpy).toHaveBeenCalledWith('copy');
  });

  it('copy shows copied state for 2s then reverts', async () => {
    vi.useFakeTimers();
    Object.defineProperty(navigator, 'clipboard', {
      value: { writeText: vi.fn().mockResolvedValue(undefined), readText: vi.fn() },
      writable: true,
      configurable: true,
    });
    render(<TerminalPanel workspaceId="w1" />);
    h.terminals[0].hasSelection.mockReturnValue(true);
    h.terminals[0].getSelection.mockReturnValue('x');
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: 'agent.terminalCopy' }));
      await Promise.resolve();
    });
    expect(screen.getByText('agent.terminalCopied')).toBeTruthy();
    act(() => {
      vi.advanceTimersByTime(2100);
    });
    expect(screen.queryByText('agent.terminalCopied')).toBeNull();
  });

  it('paste sends clipboard text as Binary frame', async () => {
    const readText = vi.fn().mockResolvedValue('paste me');
    Object.defineProperty(navigator, 'clipboard', {
      value: { writeText: vi.fn(), readText },
      writable: true,
      configurable: true,
    });
    render(<TerminalPanel workspaceId="w1" />);
    // 初始 connecting 时 paste 应 disabled；触发 open 后才可用
    expect((screen.getByRole('button', { name: 'agent.terminalPaste' }) as HTMLButtonElement).disabled).toBe(true);
    act(() => {
      wsInstance!.onopen?.();
    });
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: 'agent.terminalPaste' }));
      await Promise.resolve();
      await Promise.resolve();
    });
    // ws 已采样 paste 后的 send（binary）
    const lastSent = wsInstances[0].sent[wsInstances[0].sent.length - 1];
    expect(ArrayBuffer.isView(lastSent)).toBe(true);
    expect(new TextDecoder().decode(lastSent as Uint8Array)).toBe('paste me');
  });

  it('registers custom key handler and maps VS Code style shortcuts', () => {
    render(<TerminalPanel workspaceId="w1" />);
    expect(h.terminals[0].attachCustomKeyEventHandler).toHaveBeenCalledTimes(1);
    // 直接测纯函数，避免依赖 mock 的 handler 抓取
    const copy = vi.fn();
    const paste = vi.fn();
    const hasSelection = vi.fn(() => true);
    // Ctrl+Shift+C → 复制并阻止透传
    expect(
      handleTerminalKey(
        { ctrlKey: true, metaKey: false, shiftKey: true, key: 'C' } as unknown as KeyboardEvent,
        { copy, paste, hasSelection },
      ),
    ).toBe(false);
    expect(copy).toHaveBeenCalledTimes(1);
    // Ctrl+Shift+V → 粘贴并阻止透传
    expect(
      handleTerminalKey(
        { ctrlKey: true, metaKey: false, shiftKey: true, key: 'V' } as unknown as KeyboardEvent,
        { copy, paste, hasSelection },
      ),
    ).toBe(false);
    expect(paste).toHaveBeenCalledTimes(1);
    // Ctrl+C 有选区 → 复制并阻止透传
    copy.mockClear();
    expect(
      handleTerminalKey(
        { ctrlKey: true, metaKey: false, shiftKey: false, key: 'c' } as unknown as KeyboardEvent,
        { copy, paste, hasSelection },
      ),
    ).toBe(false);
    expect(copy).toHaveBeenCalledTimes(1);
    // Ctrl+C 无选区 → 放行 SIGINT
    copy.mockClear();
    hasSelection.mockReturnValue(false);
    expect(
      handleTerminalKey(
        { ctrlKey: true, metaKey: false, shiftKey: false, key: 'c' } as unknown as KeyboardEvent,
        { copy, paste, hasSelection },
      ),
    ).toBe(true);
    expect(copy).not.toHaveBeenCalled();
  });

  it('hot-updates theme when documentElement class toggles dark', async () => {
    render(<TerminalPanel workspaceId="w1" />);
    expect(h.terminals[0].options.theme).toEqual(TERMINAL_THEME.light);
    await act(async () => {
      document.documentElement.classList.add('dark');
      // MutationObserver 回调为微任务，等待一轮
      await Promise.resolve();
      await new Promise((r) => setTimeout(r, 0));
    });
    await waitFor(() => expect(h.terminals[0].options.theme).toEqual(TERMINAL_THEME.dark));
    await act(async () => {
      document.documentElement.classList.remove('dark');
      await Promise.resolve();
      await new Promise((r) => setTimeout(r, 0));
    });
    await waitFor(() => expect(h.terminals[0].options.theme).toEqual(TERMINAL_THEME.light));
  });
});
