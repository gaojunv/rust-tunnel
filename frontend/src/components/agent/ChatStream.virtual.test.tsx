// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach, afterEach, type Mock } from 'vitest';
import { cleanup, render, screen, act } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { listAgentMessages } from '../../api/client';
import ChatStream, { STREAM_FLUSH_MS } from './ChatStream';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('../../api/client', () => ({
  listAgentMessages: vi.fn().mockResolvedValue([]),
  updateAgentSessionModel: vi.fn().mockResolvedValue(undefined),
  getAgentDefaultModel: vi.fn().mockResolvedValue(''),
  listWorkspaceFiles: vi.fn().mockResolvedValue({ files: [] }),
  agentWsUrl: () => 'ws://test/ws',
}));

vi.mock('../../api/agentModels', () => ({
  listAgentSelectableModels: vi.fn().mockResolvedValue({ models: [], groups: [] }),
}));

let wsInstance: FakeWs | null = null;
class FakeWs {
  static OPEN = 1;
  readyState = 1;
  sent: string[] = [];
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onopen: (() => void) | null = null;
  constructor() {
    // eslint-disable-next-line @typescript-eslint/no-this-alias -- 捕获实例以便手动触发 onmessage
    wsInstance = this;
  }
  send(s: string) {
    this.sent.push(s);
  }
  close() {}
  emit(msg: object) {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }
}

/** 虚拟化路径依赖浏览器能力（ResizeObserver + 元素测量），jsdom 默认缺失导致
 *  canVirtualize=false 走全量渲染。这里安装最小 fake：scroll 容器（无 data-index）
 *  高 600px 视口、item（有 data-index）高 80px，让 virtualizer 按真实布局计算。 */
const ORIG_OFFSET_HEIGHT = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'offsetHeight');
function installVirtualMocks() {
  class FakeResizeObserver {
    private cb: (entries: { target: HTMLElement; borderBoxSize: { blockSize: number }[] }[]) => void;
    constructor(cb: (entries: { target: HTMLElement; borderBoxSize: { blockSize: number }[] }[]) => void) {
      this.cb = cb;
    }
    observe(target: HTMLElement) {
      // 同步触发一次初始测量：item（有 data-index）80px，scroll 容器 600px
      this.cb([{ target, borderBoxSize: [{ blockSize: 'index' in (target.dataset ?? {}) ? 80 : 600 }] }]);
    }
    unobserve() {}
    disconnect() {}
  }
  (globalThis as unknown as { ResizeObserver: unknown }).ResizeObserver = FakeResizeObserver;
  Object.defineProperty(HTMLElement.prototype, 'offsetHeight', {
    configurable: true,
    get(this: HTMLElement) {
      return 'index' in (this.dataset ?? {}) ? 80 : 600;
    },
  });
}
function uninstallVirtualMocks() {
  delete (globalThis as unknown as { ResizeObserver?: unknown }).ResizeObserver;
  if (ORIG_OFFSET_HEIGHT) {
    Object.defineProperty(HTMLElement.prototype, 'offsetHeight', ORIG_OFFSET_HEIGHT);
  }
}

const row = (i: number) => ({
  id: `m${i}`,
  session_id: 's1',
  role: i % 2 ? 'assistant' : 'user',
  content: `消息 ${i}`,
  tool_calls: null,
  tool_call_id: null,
  name: null,
  kind: 'message' as const,
  created_at: '2026-08-05',
});

describe('ChatStream virtualization', () => {
  beforeEach(() => {
    vi.stubGlobal('WebSocket', FakeWs as unknown as typeof WebSocket);
    wsInstance = null;
    installVirtualMocks();
  });
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    uninstallVirtualMocks();
  });

  it('renders only viewport-near items for a long conversation', async () => {
    (listAgentMessages as Mock).mockResolvedValue(Array.from({ length: 200 }, (_, i) => row(i)));
    const qc = new QueryClient({
      defaultOptions: { queries: { retry: false, refetchOnMount: false } },
    });
    render(
      <QueryClientProvider client={qc}>
        <ChatStream sessionId="s1" workspaceId="w1" model="" onModelChange={vi.fn()} />
      </QueryClientProvider>
    );
    // 顶部视口内的消息渲染出来
    expect(await screen.findByText('消息 0')).toBeTruthy();
    // 视口外（列表末尾）的消息不渲染——虚拟化生效；全量渲染会渲染全部 200 条
    expect(screen.queryByText('消息 150')).toBeNull();
    expect(screen.queryByText('消息 199')).toBeNull();
  });

  it('renders a streaming assistant bubble as plain text inside the viewport', async () => {
    // 短列表（全部在视口内）+ 视口内 append 流式气泡：验证虚拟化路径下
    // streaming 降级同样生效（Markdown 语法原样渲染为纯文本，不做 Shiki 高亮）。
    (listAgentMessages as Mock).mockResolvedValue(Array.from({ length: 5 }, (_, i) => row(i)));
    let flushCb: (() => void) | undefined;
    const origSetTimeout = globalThis.setTimeout;
    vi.spyOn(globalThis, 'setTimeout').mockImplementation(
      ((cb: () => void, ms?: number) => {
        if (ms === STREAM_FLUSH_MS) {
          flushCb = cb;
          return {} as unknown as ReturnType<typeof setTimeout>; // 手动触发，保持 streaming 状态
        }
        return origSetTimeout(cb, ms ?? 0) as ReturnType<typeof setTimeout>;
      }) as typeof setTimeout,
    );
    const qc = new QueryClient({
      defaultOptions: { queries: { retry: false, refetchOnMount: false } },
    });
    render(
      <QueryClientProvider client={qc}>
        <ChatStream sessionId="s1" workspaceId="w1" model="" onModelChange={vi.fn()} />
      </QueryClientProvider>
    );
    await screen.findByText('消息 0');
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '# 标题', final: false });
    });
    act(() => {
      flushCb?.();
    });
    // 纯文本渲染：Markdown 渲染会把 `# 标题` 解析成 <h1>（文本为"标题"），
    // streaming 降级则保留原始文本串
    expect(screen.getByText('# 标题')).toBeTruthy();
  });
});
