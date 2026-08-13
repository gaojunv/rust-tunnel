// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach, afterEach, type Mock } from 'vitest';
import { cleanup, render, screen, act, fireEvent } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { listAgentMessages, listWorkspaceFiles } from '../../api/client';
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

// 捕获 ws 实例以便手动触发 onmessage
const wsInstances: FakeWs[] = [];
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
    wsInstances.push(this);
  }
  send(s: string) {
    this.sent.push(s);
  }
  close() {}
  emit(msg: object) {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }
}

const renderChat = () => {
  const qc = new QueryClient({
    defaultOptions: {
      // refetchOnMount:false — ChatStream 的 history effect 依赖「挂载时只装载一次」
      // （done/重连后显式 invalidate 才会重新装载）。默认 refetchOnMount 会让 WS
      // effect 触发的无关 state 更新也引发 refetch → 覆盖聊天区实时增量。
      queries: { retry: false, refetchOnMount: false },
    },
  });
  return render(
    <QueryClientProvider client={qc}>
      <ChatStream sessionId="s1" workspaceId="w1" model="" onModelChange={vi.fn()} />
    </QueryClientProvider>
  );
};

describe('ChatStream running state', () => {
  beforeEach(() => {
    vi.stubGlobal('WebSocket', FakeWs as unknown as typeof WebSocket);
    wsInstance = null;
    wsInstances.length = 0;
  });
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it('stays running after tool_call, clears on tool_result + done', async () => {
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    act(() => {
      wsInstance!.emit({ type: 'tool_result', id: 'c1', name: 'list_dir', result: 'ok' });
    });
    // tool 回齐但未收到 done → 仍 Running
    expect(screen.getByText('agent.running')).toBeTruthy();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '完成' });
      wsInstance!.emit({ type: 'done' });
    });
    expect(screen.queryByText('agent.running')).toBeNull();
  });

  it('clears running on error even with pending tools', async () => {
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    act(() => {
      wsInstance!.emit({ type: 'error', message: 'boom' });
    });
    expect(screen.queryByText('agent.running')).toBeNull();
  });

  it('clears running on done even with lost tool_result frames', async () => {
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    // tool_result 帧丢失（断线场景），done 到达即应解除 running
    act(() => {
      wsInstance!.emit({ type: 'done' });
    });
    expect(screen.queryByText('agent.running')).toBeNull();
  });

  it('reconnects after close and shows reconnecting banner', async () => {
    // 重连退避首次 1s → 本测试内压缩到 1ms 以同步触发（spy 随 afterEach 恢复）
    const origSetTimeout = globalThis.setTimeout;
    vi.spyOn(globalThis, 'setTimeout').mockImplementation(((cb: () => void, ms?: number) => {
      return origSetTimeout(cb, ms !== undefined && ms >= 1000 && ms <= 15000 ? 1 : ms) as ReturnType<typeof setTimeout>;
    }) as typeof setTimeout);
    renderChat();
    // SessionSettingsMenu 的 models 查询在渲染期间 resolve → 触发一次重渲染 →
    // WS effect 随之重建（i18n mock 的 t 每次渲染返回新引用），实例数不再恒为 1。
    // wsInstance 总指向最后一次 connect 创建的活跃连接，以它触发断线。
    const active = wsInstance!;
    expect(active).toBeTruthy();
    act(() => {
      active.onclose?.();
    });
    // 断线横幅出现
    expect(screen.getByText('agent.reconnecting')).toBeTruthy();
    // 退避（测试内 1ms）后自动重连
    await act(async () => {
      await new Promise((r) => origSetTimeout(r, 20));
    });
    // 重连创建了新的活跃实例（而非复用旧连接）
    const reconnected = wsInstance!;
    expect(reconnected).not.toBe(active);
    act(() => {
      reconnected.onopen?.();
    });
    expect(screen.queryByText('agent.reconnecting')).toBeNull();
  });

  it('warns about possibly-lost message when closed mid-run', async () => {
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    // tool_call 引发状态更新 → React Query 对 stale 查询后台 refetch → 挂载过
    // 的 history effect 重新执行（loadedRef 已 true，直接跳过），但 WS effect
    // 不会因此重建——取「当前活跃连接」（wsInstance）触发关闭。
    act(() => {
      wsInstance!.onclose?.();
    });
    // running 解除 + 中断提示（刚发的消息可能未处理）
    expect(screen.queryByText('agent.running')).toBeNull();
    expect(screen.getByText(/agent.connectionInterrupted/)).toBeTruthy();
  });

  it('force-clears running after 10min timeout', async () => {
    // 说明：vitest v4 + jsdom 环境下，组件模块内部调用的 setTimeout 不会被
    // vi.advanceTimersByTime 驱动（见 task-9 报告），因此改为 spy 捕获 10 分钟
    // 超时回调并确定性触发；断言语义不变：超时兜底必须无条件解除 Running。
    let timeoutCb: (() => void) | undefined;
    const origSetTimeout = globalThis.setTimeout;
    vi.spyOn(globalThis, 'setTimeout').mockImplementation(
      ((cb: () => void, ms?: number) => {
        if (ms === 10 * 60 * 1000) timeoutCb = cb;
        return origSetTimeout(cb, ms ?? 0) as ReturnType<typeof setTimeout>;
      }) as typeof setTimeout,
    );
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    act(() => {
      timeoutCb?.();
    });
    expect(screen.queryByText('agent.running')).toBeNull();
  });

  it('renders new-format tool_calls/tool_result history', async () => {
    (listAgentMessages as Mock).mockResolvedValue([
      { id: 'm1', session_id: 's1', role: 'user', content: '看下文件', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-05' },
      { id: 'm2', session_id: 's1', role: 'assistant', content: '', tool_calls: JSON.stringify([{ id: 'c1', type: 'function', function: { name: 'read_file', arguments: '{"path":"a.rs"}' } }]), tool_call_id: null, name: null, kind: 'tool_calls', created_at: '2026-08-05' },
      { id: 'm3', session_id: 's1', role: 'tool', content: 'fn main(){}', tool_calls: null, tool_call_id: 'c1', name: 'read_file', kind: 'tool_result', created_at: '2026-08-05' },
      { id: 'm4', session_id: 's1', role: 'assistant', content: '文件里是 main 函数', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-05' },
    ]);
    renderChat();
    // 工具名、参数、结果都渲染出来（工具卡片默认收起，先点头部展开再断言 args/result）；
    // read_file 归一化为规范名 Read
    expect(await screen.findByText('Read')).toBeTruthy();
    // 工具卡片头（含工具名的按钮，aria-expanded）点击展开；SessionSettingsMenu
    // 触发器同样带 aria-expanded=false，故不再用 getByRole({expanded:false})
    fireEvent.click(screen.getByText('Read').closest('button')!);
    expect(screen.getByText(/fn main\(\)/)).toBeTruthy();
    expect(screen.getByText('文件里是 main 函数')).toBeTruthy();
  });

  it('renders legacy-format tool row with kind=message role=tool', async () => {
    // 迁移前遗留行：SQLite ALTER TABLE DEFAULT 补 role='tool' 但 kind='message'
    (listAgentMessages as Mock).mockResolvedValue([
      { id: 'm1', session_id: 's1', role: 'tool', content: '', tool_calls: JSON.stringify([{ name: 'shell', args: '{"cmd":"ls"}', result: 'a.rs' }]), tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-05' },
    ]);
    renderChat();
    // shell 归一化为规范名 Terminal
    expect(await screen.findByText('Terminal')).toBeTruthy();
    // 工具卡片默认收起，先展开（含工具名的卡片头按钮）再断言结果
    fireEvent.click(screen.getByText('Terminal').closest('button')!);
    expect(screen.getByText('a.rs')).toBeTruthy();
  });

  it('renders orphan tool_calls row as failed card (turn interrupted mid-tool)', async () => {
    // 回合在工具执行中被刷新/断线打断：tool_call 已落库，tool_result 永不到达。
    // 重载后该行若无卡片兜底，工具从聊天区彻底消失（现象：无标题无内容的卡片，
    // 或少一段）。渲染为 failed 占位卡片（保留工具名，状态 ✗）。
    (listAgentMessages as Mock).mockResolvedValue([
      { id: 'm1', session_id: 's1', role: 'user', content: '看下目录', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-05' },
      { id: 'm2', session_id: 's1', role: 'assistant', content: '', tool_calls: JSON.stringify([{ id: 'c1', name: 'list_dir', arguments: '{"path":"."}' }]), tool_call_id: 'c1', name: 'list_dir', kind: 'tool_calls', created_at: '2026-08-05' },
    ]);
    renderChat();
    // 孤儿 tool_calls 行渲染为 failed 卡片：工具名可见（list_dir 归一化为 Read）、状态徽章 ✗
    expect(await screen.findByText('Read')).toBeTruthy();
    // StatusBadge 对 failed 渲染 ✗（折叠卡片头部可见，无需展开）
    expect(screen.getByText('✗')).toBeTruthy();
    // 重载时末尾是 tool_calls 行 → running 兜底置 true（回合可能仍在服务端跑）
    expect(screen.getByText('agent.running')).toBeTruthy();
  });

  it('does not render orphan card for tool_calls with paired tool_result', async () => {
    // 正常完成的工具：tool_calls 行不渲染（args 由 tool_result 卡片展示），
    // 只出现一张卡片，不重复。
    (listAgentMessages as Mock).mockResolvedValue([
      { id: 'm1', session_id: 's1', role: 'user', content: '看下目录', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-05' },
      { id: 'm2', session_id: 's1', role: 'assistant', content: '', tool_calls: JSON.stringify([{ id: 'c1', name: 'list_dir', arguments: '{"path":"."}' }]), tool_call_id: 'c1', name: 'list_dir', kind: 'tool_calls', created_at: '2026-08-05' },
      { id: 'm3', session_id: 's1', role: 'tool', content: 'src/ tests/', tool_calls: null, tool_call_id: 'c1', name: 'list_dir', kind: 'tool_result', created_at: '2026-08-05' },
    ]);
    renderChat();
    // 工具名归一化为 Read（list_dir 为 read 类别别名）
    expect(await screen.findByText('Read')).toBeTruthy();
    // 只有一张卡片（tool_result 渲染的），孤儿兜底不触发
    expect(screen.getAllByText('Read')).toHaveLength(1);
    // 卡片状态 ✓（completed）
    expect(screen.getByText('✓')).toBeTruthy();
  });

  it('renders runner orphan tool_calls (column tool_call_id null) as failed card', async () => {
    // runner 旧格式：tool_calls 行整行 tool_call_id 列为 null，但 JSON 内每个
    // 调用带 id。回合在工具执行中被取消（tool_result 永不到达）时，若只认列
    // 上的 tool_call_id，这些工具刷新后会从聊天区消失。按 JSON 内 id 与
    // tool_result 配对，未配对的渲染 failed 占位卡。
    (listAgentMessages as Mock).mockResolvedValue([
      { id: 'm1', session_id: 's1', role: 'user', content: '看下目录', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-05' },
      { id: 'm2', session_id: 's1', role: 'assistant', content: '', tool_calls: JSON.stringify([{ id: 'c1', name: 'list_dir', arguments: '{"path":"."}' }]), tool_call_id: null, name: null, kind: 'tool_calls', created_at: '2026-08-05' },
    ]);
    renderChat();
    expect(await screen.findByText('Read')).toBeTruthy();
    expect(screen.getByText('✗')).toBeTruthy();
  });

  it('dedups live tool_call against history orphan card (no duplicate)', async () => {
    // 刷新后 live tool_call 与 history 已渲染的孤儿卡片是同一工具（tool_call
    // 已落库、tool_result 未到）。按 toolId 就地升级状态，不追加第二张卡——
    // 否则 tool_result 只 patch 一张，另一张永远 running（Bug 复现）。
    (listAgentMessages as Mock).mockResolvedValue([
      { id: 'm1', session_id: 's1', role: 'user', content: '看下目录', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-05' },
      { id: 'm2', session_id: 's1', role: 'assistant', content: '', tool_calls: JSON.stringify([{ id: 'c1', name: 'list_dir', arguments: '{"path":"."}' }]), tool_call_id: 'c1', name: 'list_dir', kind: 'tool_calls', created_at: '2026-08-05' },
    ]);
    renderChat();
    // 半截装载：孤儿卡（failed ✗）已渲染
    expect(await screen.findByText('Read')).toBeTruthy();
    expect(screen.getByText('✗')).toBeTruthy();
    // live tool_call 同 id 到达：就地升级为运行中，不新增卡片
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', tool_kind: 'read', status: 'in_progress' });
    });
    expect(screen.getAllByText('Read')).toHaveLength(1);
    expect(screen.queryByText('✗')).toBeNull();
    // 结果到达：按 toolId 精确匹配 → 完成
    act(() => {
      wsInstance!.emit({ type: 'tool_result', id: 'c1', name: 'list_dir', status: 'completed', result: 'src/' });
    });
    expect(screen.getByText('✓')).toBeTruthy();
    expect(screen.getAllByText('Read')).toHaveLength(1);
  });

  it('reconciles complete history on done after a mid-turn partial load', async () => {
    // 半截装载（刷新时回合仍在跑）：DB 当时缺终态 flush 的文本/结果。done
    // 到达后服务端已完整落库——重置 loadedRef 让 refetch 重渲染完整历史：
    // 孤儿卡变 completed、终态文本补全、running 兜底不复发（对账重载跳过 heuristic）。
    const orphanCalls = JSON.stringify([{ id: 'c1', name: 'list_dir', arguments: '{"path":"."}' }]);
    const partial = [
      { id: 'm1', session_id: 's1', role: 'user', content: '看下目录', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-08' },
      { id: 'm2', session_id: 's1', role: 'assistant', content: '', tool_calls: orphanCalls, tool_call_id: 'c1', name: 'list_dir', kind: 'tool_calls', created_at: '2026-08-08' },
    ];
    const complete = [
      ...partial,
      { id: 'm3', session_id: 's1', role: 'tool', content: 'src/ tests/', tool_calls: null, tool_call_id: 'c1', name: 'list_dir', kind: 'tool_result', created_at: '2026-08-08' },
      { id: 'm4', session_id: 's1', role: 'assistant', content: '完成', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-08' },
    ];
    (listAgentMessages as Mock).mockResolvedValueOnce(partial).mockResolvedValue(complete);
    renderChat();
    // 半截装载：孤儿卡 failed + running 兜底
    expect(await screen.findByText('Read')).toBeTruthy();
    expect(screen.getByText('✗')).toBeTruthy();
    expect(screen.getByText('agent.running')).toBeTruthy();
    // done → invalidate → refetch 返回完整历史 → 对账重载
    await act(async () => {
      wsInstance!.emit({ type: 'done' });
    });
    expect(await screen.findByText('完成')).toBeTruthy();
    expect(screen.getByText('✓')).toBeTruthy();
    // running 兜底不复发（对账重载跳过 running heuristic）
    expect(screen.queryByText('agent.running')).toBeNull();
  });

  it('merges streamed assistant_chunk deltas into one bubble', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '你好', final: false });
      wsInstance!.emit({ type: 'assistant_chunk', content: '，世界', final: false });
      wsInstance!.emit({ type: 'assistant_chunk', content: '', final: true });
    });
    // 一个气泡，内容为拼接结果
    const bubbles = screen.getAllByText('你好，世界');
    expect(bubbles).toHaveLength(1);
  });

  it('renders status event as transient hint', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'status', message: 'compacting' });
    });
    expect(await screen.findByText(/compacting|压缩/)).toBeTruthy();
  });

  it('renders non-SSE fallback (content + final in one chunk) as a single bubble', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '非流式回退完整文本', final: true });
    });
    // 一条 content+final:true 消息：先追加内容再关闭气泡 → 单个完整气泡
    const bubbles = screen.getAllByText('非流式回退完整文本');
    expect(bubbles).toHaveLength(1);
  });

  it('stream_reset 清空半截流式气泡并保留后续新流', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      // 流式增量 → stream_reset → 状态提示 → 重试后的完整文本
      wsInstance!.emit({ type: 'assistant_chunk', content: '半截内容', final: false });
      wsInstance!.emit({ type: 'stream_reset' });
      wsInstance!.emit({ type: 'status', message: '上游连接中断，正在重试 (1/2)' });
      wsInstance!.emit({ type: 'assistant_chunk', content: '完整内容', final: true });
    });
    // 半截内容被丢弃：界面上只出现「正在重试」提示 + 完整内容气泡
    expect(screen.getByText(/上游连接中断，正在重试/)).toBeTruthy();
    expect(screen.getByText('完整内容')).toBeTruthy();
    expect(screen.queryByText('半截内容')).toBeNull();
  });

  it('stream_reset 真正移除已 flush 实体化的半截气泡（定时 flush 后重置）', () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    // 捕获 STREAM_FLUSH_MS 定时器回调并手动触发：真实浏览器里 50ms 定时器在
    // 流式期间必然已触发，半截早已实体化为可见气泡，stream_reset 必须按 idx
    // 真正移除它（不能只断开流式引用）。task-9 报告：vitest v4 + jsdom 下
    // vi.advanceTimersByTime 驱动不了模块内 setTimeout，故用 spy 捕获手动触发
    // （与「10 分钟超时」测试同款手法）。帧序与真实服务端一致：半截 chunk →
    // stream_reset → status(重试提示) → 完整 chunk（见 runner.rs 传输层失败重试）。
    let flushCb: (() => void) | undefined;
    const origSetTimeout = globalThis.setTimeout;
    vi.spyOn(globalThis, 'setTimeout').mockImplementation(
      ((cb: () => void, ms?: number) => {
        if (ms === STREAM_FLUSH_MS) {
          flushCb = cb;
          return {} as unknown as ReturnType<typeof setTimeout>; // 不真正调度，测试手动触发
        }
        return origSetTimeout(cb, ms ?? 0) as ReturnType<typeof setTimeout>;
      }) as typeof setTimeout,
    );
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '半截内容', final: false });
    });
    // 模拟 STREAM_FLUSH_MS 后定时 flush：半截内容实体化为可见气泡
    act(() => {
      flushCb?.();
    });
    expect(screen.getByText('半截内容')).toBeTruthy();
    act(() => {
      wsInstance!.emit({ type: 'stream_reset' });
      wsInstance!.emit({ type: 'status', message: '上游连接中断，正在重试 (1/2)' });
      wsInstance!.emit({ type: 'assistant_chunk', content: '完整内容', final: true });
    });
    // 半截气泡被真正移除：界面上只剩重试提示 + 完整内容
    expect(screen.getByText(/上游连接中断，正在重试/)).toBeTruthy();
    expect(screen.getByText('完整内容')).toBeTruthy();
    expect(screen.queryByText('半截内容')).toBeNull();
  });

  it('does not fragment the trailing text of a streamed turn (M1)', () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    // M1 回归：`flushChunks(); breakStream();` 若同步置空 streamingIdxRef，会在
    // flushChunks 的 setItems updater 执行前读到 null → 工具边界处缓冲尾文本
    // 新建碎片气泡（「前文」与「续文」分裂）。捕获 STREAM_FLUSH_MS 定时器先把
    // 「前文」实体化为气泡，再缓冲「续文」后触发 tool_call 边界——「续文」必须
    // 并入「前文」气泡（工具卡片之前），不得与「后文」合并成独立气泡。
    let flushCb: (() => void) | undefined;
    const origSetTimeout = globalThis.setTimeout;
    vi.spyOn(globalThis, 'setTimeout').mockImplementation(
      ((cb: () => void, ms?: number) => {
        if (ms === STREAM_FLUSH_MS) {
          flushCb = cb;
          return {} as unknown as ReturnType<typeof setTimeout>;
        }
        return origSetTimeout(cb, ms ?? 0) as ReturnType<typeof setTimeout>;
      }) as typeof setTimeout,
    );
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '前文', final: false });
    });
    // 定时 flush 实体化「前文」气泡（streamingIdxRef 指向它）
    act(() => {
      flushCb?.();
    });
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '续文', final: false });
    });
    // 工具边界：缓冲的「续文」必须 flush 进「前文」气泡，而非新建碎片
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '后文', final: true });
    });
    // 「续文」并入「前文」→ 「前文续文」整体存在；碎片化时「续文后文」合并出现
    expect(screen.getByText('前文续文')).toBeTruthy();
    expect(screen.queryByText('续文后文')).toBeNull();
  });

  it('status closes the current streaming bubble before appending the hint', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '流式', final: false });
      wsInstance!.emit({ type: 'status', message: 'compacting' });
      wsInstance!.emit({ type: 'assistant_chunk', content: '后续', final: false });
      wsInstance!.emit({ type: 'assistant_chunk', content: '', final: true });
    });
    // status 断开流式气泡：'流式' 与 '后续' 各自独立，不合并
    expect(screen.getByText('流式')).toBeTruthy();
    expect(screen.getByText('后续')).toBeTruthy();
    expect(screen.queryByText('流式后续')).toBeNull();
  });

  it('dedups re-inserted kept segment after compaction (M3)', async () => {
    // DB 物理顺序：[旧消息..., 原kept..., summary, 重插kept...]——压缩修复
    // （801c9a6）使 kept 段以相同内容出现两次，前端必须只渲染一份。
    // K = summary 后行数（含 tool_calls/tool_result 行），跳过 summary 前最后 K 行。
    const row = (id: string, role: string, content: string, kind: string) => ({
      id,
      session_id: 's1',
      role,
      content,
      tool_calls: null,
      tool_call_id: null,
      name: null,
      kind,
      created_at: '2026-08-05',
    });
    const toolCalls = JSON.stringify([
      { id: 'c1', type: 'function', function: { name: 'read_file', arguments: '{"path":"a.rs"}' } },
    ]);
    const toolCallsRow = (id: string) => ({
      ...row(id, 'assistant', '', 'tool_calls'),
      tool_calls: toolCalls,
    });
    const toolResultRow = (id: string) => ({
      ...row(id, 'tool', 'fn main(){}', 'tool_result'),
      tool_call_id: 'c1',
      name: 'read_file',
    });
    (listAgentMessages as Mock).mockResolvedValue([
      row('old1', 'user', '早期问题', 'message'),
      row('old2', 'assistant', '早期回答', 'message'),
      // 原 kept 段（summary 前，含 tool 配对行）——应被跳过
      row('k1', 'user', '保留问题', 'message'),
      toolCallsRow('k2'),
      toolResultRow('k3'),
      row('sum', 'user', '[上下文摘要] 之前讨论了 A', 'summary'),
      // 重插 kept 段（summary 后）——只渲染这一份
      row('k1r', 'user', '保留问题', 'message'),
      toolCallsRow('k2r'),
      toolResultRow('k3r'),
    ]);
    renderChat();
    // 旧消息与 summary 完整保留
    expect(await screen.findByText('早期问题')).toBeTruthy();
    expect(screen.getByText('早期回答')).toBeTruthy();
    expect(screen.getByText('[上下文摘要] 之前讨论了 A')).toBeTruthy();
    // 重插的 kept 段只渲染一次（原始 kept 行被跳过，无连续重复段）
    expect(screen.getAllByText('保留问题')).toHaveLength(1);
    // 工具卡片同样只渲染一份（read_file 归一化为 Read + 结果；默认收起，先展开再断言结果）
    expect(screen.getAllByText('Read')).toHaveLength(1);
    fireEvent.click(screen.getByText('Read').closest('button')!);
    expect(screen.getAllByText(/fn main\(\)/)).toHaveLength(1);
  });

  it('keeps legit history when new messages follow compaction (over-skip fix)', async () => {
    // 压缩后用户继续对话：DB 顺序 [..., 原kept, summary, 重插kept, 新消息...]。
    // 旧去重逻辑把「summary 后行数」当作重插行数，会多跳掉 summary 前没有重复
    // 副本的合法旧行；内容匹配去重应只跳过真正的重复原件。
    const row = (id: string, role: string, content: string, kind: string) => ({
      id,
      session_id: 's1',
      role,
      content,
      tool_calls: null,
      tool_call_id: null,
      name: null,
      kind,
      created_at: '2026-08-05',
    });
    (listAgentMessages as Mock).mockResolvedValue([
      row('old1', 'user', '最早的问题', 'message'),
      row('old2', 'assistant', '最早的回答', 'message'),
      row('k1', 'user', '保留问题', 'message'),
      row('k2', 'assistant', '保留回答', 'message'),
      row('sum', 'user', '[上下文摘要] 之前讨论了 A', 'summary'),
      row('k1r', 'user', '保留问题', 'message'),
      row('k2r', 'assistant', '保留回答', 'message'),
      // 压缩之后的新对话（排在 summary 后，但不是重插副本）
      row('new1', 'user', '压缩后的新问题', 'message'),
      row('new2', 'assistant', '压缩后的新回答', 'message'),
    ]);
    renderChat();
    // 合法旧消息不能被多跳掉
    expect(await screen.findByText('最早的问题')).toBeTruthy();
    expect(screen.getByText('最早的回答')).toBeTruthy();
    // 重插 kept 只渲染一份
    expect(screen.getAllByText('保留问题')).toHaveLength(1);
    expect(screen.getAllByText('保留回答')).toHaveLength(1);
    // 压缩后的新消息正常渲染
    expect(screen.getByText('压缩后的新问题')).toBeTruthy();
    expect(screen.getByText('压缩后的新回答')).toBeTruthy();
  });

  it('running 时显示停止按钮，点击发送 cancel 并解除 running', async () => {
    renderChat();
    // 进入 running：与既有 running 测试一致，用 tool_call 帧驱动 armRunning
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    // 停止按钮（aria-label = t('agent.stop')）：running 期间与发送按钮并存
    const stopBtn = screen.getByRole('button', { name: 'agent.stop' });
    expect(stopBtn).toBeTruthy();
    // 捕获当前活跃连接：i18n mock 的 t 每次渲染返回新引用 → armRunning（WS effect
    // 依赖）不稳定 → 每次 state 更新都重建 WebSocket（实例轮换），点击后 wsInstance
    // 已指向更新的实例；在点击前捕获当前实例再断言其 send。
    const ws = wsInstance!;
    act(() => {
      stopBtn.click();
    });
    // mockWs.send 被调用且 payload 含 '"type":"cancel"'
    expect(ws.sent.some((s) => s.includes('"type":"cancel"'))).toBe(true);
    // running 指示消失 + 停止提示气泡出现
    expect(screen.queryByText('agent.running')).toBeNull();
    expect(screen.getByText(/agent.stopped/)).toBeTruthy();
  });

  it('keeps send button usable while running (queues instead of dropping)', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // 进入 running：回合进行中用户仍可提交消息（服务端 busy 时排队，回 queued 帧）
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    // 停止按钮与发送按钮并存
    expect(screen.getByRole('button', { name: 'agent.stop' })).toBeTruthy();
    const sendBtn = screen.getByRole('button', { name: 'agent.send' }) as HTMLButtonElement;
    // 无输入时发送按钮仍禁用
    expect(sendBtn.disabled).toBe(true);
    // 输入后可用
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: '排队消息' } });
    expect((screen.getByRole('button', { name: 'agent.send' }) as HTMLButtonElement).disabled).toBe(false);
    // 点击发送：照常发 user_message 帧（服务端 busy 会排队而非丢弃）
    const ws = wsInstance!;
    fireEvent.click(screen.getByRole('button', { name: 'agent.send' }));
    expect(ws.sent.some((s) => s.includes('"type":"user_message"') && s.includes('排队消息'))).toBe(true);
    // running 状态保持（当前回合仍在服务端执行）
    expect(screen.getByText('agent.running')).toBeTruthy();
  });

  it('shows queued hint on queued frame while running', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    // 服务端 busy 入队确认帧 → 轻量提示气泡
    act(() => {
      wsInstance!.emit({ type: 'queued' });
    });
    expect(screen.getByText('agent.messageQueued')).toBeTruthy();
    // 不打断进行中的回合
    expect(screen.getByText('agent.running')).toBeTruthy();
  });

  it('shows cancel_fallback warning and clears running', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    // 停止超时兜底：agent 未响应停止，服务端强制杀进程并重启
    act(() => {
      wsInstance!.emit({ type: 'cancel_fallback' });
    });
    expect(screen.queryByText('agent.running')).toBeNull();
    expect(screen.getByText('agent.cancelFallback')).toBeTruthy();
  });

  it('keeps Mode/Effort config buttons enabled while running', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // 注入 session_state（mode 项）→ Mode 快捷按钮出现
    act(() => {
      wsInstance!.emit({
        type: 'session_state',
        options: [
          {
            id: 'mode', name: 'Mode', category: 'mode', type: 'select',
            currentValue: 'plan',
            options: [{ value: 'plan', name: 'Plan' }],
          },
        ],
      });
    });
    const modeBtn = screen.getByRole('button', { name: 'agent.configMode' });
    expect((modeBtn as HTMLButtonElement).disabled).toBe(false);
    // 进入 running 后 mode 快捷按钮仍可用（运行中允许改 mode/effort/模型）
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    expect((screen.getByRole('button', { name: 'agent.configMode' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('flushes buffered text before appending stopped bubble on cancel (M11)', () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'shell', args: '{}' });
    });
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '正在执行…', final: false });
    });
    // 在 50ms flush 定时器触发前点击停止：缓冲尾文本必须先实体化，再落停止提示。
    // 若不 flush，停止提示会先于尾文本出现（尾文本 50ms 后才落屏，顺序颠倒）。
    const stopBtn = screen.getByRole('button', { name: 'agent.stop' });
    act(() => {
      stopBtn.click();
    });
    // 流式文本已同步 flush（停止提示之前可见）
    expect(screen.getByText('正在执行…')).toBeTruthy();
    expect(screen.getByText(/agent.stopped/)).toBeTruthy();
  });

  it('收到 stopped 帧解除 running', async () => {
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    // 服务端确认取消（本连接或其他标签页的 cancel 都经 WS 广播 stopped）
    act(() => {
      wsInstance!.emit({ type: 'stopped' });
    });
    expect(screen.queryByText('agent.running')).toBeNull();
  });

  it('invalidates agent-sessions cache on session_title frame', async () => {
    // 标题生成晚于 done 帧（需数秒），done 时 refetch 早于标题写库——服务端
    // 写库后另发 session_title 帧，前端据此刷新会话列表让 SessionBar 回显。
    // 与 done 帧的 invalidate 同一判定方式：spy QueryClient.invalidateQueries。
    (listAgentMessages as Mock).mockResolvedValue([]);
    const qc = new QueryClient({
      defaultOptions: { queries: { retry: false, refetchOnMount: false } },
    });
    const invalidateSpy = vi.spyOn(qc, 'invalidateQueries');
    render(
      <QueryClientProvider client={qc}>
        <ChatStream sessionId="s1" workspaceId="w1" model="" onModelChange={vi.fn()} />
      </QueryClientProvider>
    );
    act(() => {
      wsInstance!.emit({ type: 'session_title', title: '修复登录 bug', session_id: 's1' });
    });
    expect(invalidateSpy).toHaveBeenCalledWith({ queryKey: ['agent-sessions'] });
  });

  it('renders summary rows as assistant bubbles (M5)', async () => {
    const row = (id: string, role: string, content: string, kind: string) => ({
      id,
      session_id: 's1',
      role,
      content,
      tool_calls: null,
      tool_call_id: null,
      name: null,
      kind,
      created_at: '2026-08-05',
    });
    (listAgentMessages as Mock).mockResolvedValue([
      row('u', 'user', '普通用户消息', 'message'),
      row('s', 'user', '[上下文摘要] 之前讨论了 X', 'summary'),
    ]);
    renderChat();
    const userEl = await screen.findByText('普通用户消息');
    const summaryEl = screen.getByText('[上下文摘要] 之前讨论了 X');
    // 新布局：user 为右对齐小气泡（ml-auto + bg-primary/10）；summary 走
    // assistant 全宽正文（无气泡，Streamdown 渲染为带 data-streamdown 标记的 DOM）
    const userBubble = userEl.closest('[class*="ml-auto"]');
    expect(userBubble?.className).toContain('bg-primary/10');
    expect(summaryEl.closest('[class*="ml-auto"]')).toBeNull();
  });

  it('renders approval card and responds on approve', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // 注入 approval_request 帧：卡片应出现（标题 + 工具名 + 摘要）
    act(() => {
      wsInstance!.emit({
        type: 'approval_request',
        request_id: 'req1',
        tool: 'shell',
        summary: 'rm -rf /tmp/x',
        args_preview: '{"cmd":"rm -rf /tmp/x"}',
      });
    });
    // 标题文案后紧跟冒号与工具名（跨元素），用子串匹配
    expect(screen.getByText(/agent\.approvalRequired/)).toBeTruthy();
    expect(screen.getByText('shell')).toBeTruthy();
    expect(screen.getByText('rm -rf /tmp/x')).toBeTruthy();
    // 三个操作按钮齐全（mock t 返回 key 作为按钮文案）
    expect(screen.getByRole('button', { name: 'agent.approveOnce' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'agent.approveSession' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'agent.deny' })).toBeTruthy();
    // 点击「允许一次」→ 捕获当前连接，断言发出 approval_response
    const ws = wsInstance!;
    fireEvent.click(screen.getByRole('button', { name: 'agent.approveOnce' }));
    expect(
      ws.sent.some(
        (s) =>
          s.includes('"type":"approval_response"') &&
          s.includes('"request_id":"req1"') &&
          s.includes('"approved":true') &&
          s.includes('"remember":"none"'),
      ),
    ).toBe(true);
    // 卡片变为已允许：操作按钮消失、状态文案出现
    expect(screen.queryByRole('button', { name: 'agent.approveOnce' })).toBeNull();
    expect(screen.getByText(/agent.approved/)).toBeTruthy();
  });

  it('denies approval and approve-session sends remember=session', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'approval_request', request_id: 'req2', tool: 'shell', summary: 'echo hi', args_preview: '{}' });
    });
    // 拒绝：approved=false, remember=none，卡片变为已拒绝
    const ws = wsInstance!;
    fireEvent.click(screen.getByRole('button', { name: 'agent.deny' }));
    expect(
      ws.sent.some((s) => s.includes('"type":"approval_response"') && s.includes('"request_id":"req2"') && s.includes('"approved":false') && s.includes('"remember":"none"')),
    ).toBe(true);
    expect(screen.getByText(/agent.denied/)).toBeTruthy();
    // 新的审批请求 → 点击「本会话允许」：remember=session
    act(() => {
      wsInstance!.emit({ type: 'approval_request', request_id: 'req3', tool: 'write_file', summary: 'write x', args_preview: '{}' });
    });
    const ws2 = wsInstance!;
    fireEvent.click(screen.getByRole('button', { name: 'agent.approveSession' }));
    expect(
      ws2.sent.some((s) => s.includes('"type":"approval_response"') && s.includes('"request_id":"req3"') && s.includes('"approved":true') && s.includes('"remember":"session"')),
    ).toBe(true);
    expect(screen.getByText(/agent.approved/)).toBeTruthy();
  });

  it('renders ACP permission options and returns selected option_id', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // approval_request 携带 options：卡片渲染选项按钮（而非 approveOnce/deny 二元）
    act(() => {
      wsInstance!.emit({
        type: 'approval_request',
        request_id: 'req4',
        tool: 'shell',
        summary: 'run a script',
        args_preview: '{}',
        options: [
          { id: 'allow_once', label: '允许一次', kind: 'allow_once' },
          { id: 'allow_always', label: '总是允许', kind: 'allow_always' },
          { id: 'reject', label: '拒绝', kind: 'reject_once' },
        ],
      });
    });
    expect(screen.getByRole('button', { name: /允许一次/ })).toBeTruthy();
    expect(screen.getByRole('button', { name: /总是允许/ })).toBeTruthy();
    expect(screen.getByRole('button', { name: /拒绝/ })).toBeTruthy();
    // 有选项时不显示二元按钮
    expect(screen.queryByRole('button', { name: 'agent.approveOnce' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'agent.deny' })).toBeNull();
    // 点击 allow_always → 回传 option_id（原样）+ remember=session
    const ws = wsInstance!;
    fireEvent.click(screen.getByRole('button', { name: /总是允许/ }));
    expect(
      ws.sent.some(
        (s) =>
          s.includes('"type":"approval_response"') &&
          s.includes('"request_id":"req4"') &&
          s.includes('"option_id":"allow_always"') &&
          s.includes('"remember":"session"'),
      ),
    ).toBe(true);
    // 卡片变为已允许
    expect(screen.getByText(/agent.approved/)).toBeTruthy();
  });

  it('expires pending approval cards on done frame and unlocks send', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({
        type: 'approval_request',
        request_id: 'req1',
        tool: 'shell',
        summary: 'rm -rf /tmp/x',
        args_preview: '{}',
      });
    });
    // 输入文本后发送按钮仍被 pending 审批禁用
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: 'hi' } });
    expect((screen.getByRole('button', { name: 'agent.send' }) as HTMLButtonElement).disabled).toBe(true);
    // done 帧到达（服务端 5 分钟审批超时按 deny 继续回合）→ 卡片过期、发送解锁
    act(() => {
      wsInstance!.emit({ type: 'done' });
    });
    expect(screen.queryByRole('button', { name: 'agent.approveOnce' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'agent.deny' })).toBeNull();
    expect(screen.getByText('agent.approvalExpired')).toBeTruthy();
    expect((screen.getByRole('button', { name: 'agent.send' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('expires pending approval cards on stop and unlocks send', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // 危险工具调用进入 running → 服务端发审批请求挂起回合
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'shell', args: '{}' });
    });
    act(() => {
      wsInstance!.emit({
        type: 'approval_request',
        request_id: 'req2',
        tool: 'shell',
        summary: 'rm -rf /tmp/x',
        args_preview: '{}',
      });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    // 点击前捕获当前连接（stop 触发 state 更新后 WS 实例轮换，cancel 发在旧实例）
    const ws = wsInstance!;
    act(() => {
      screen.getByRole('button', { name: 'agent.stop' }).click();
    });
    expect(ws.sent.some((s) => s.includes('"type":"cancel"'))).toBe(true);
    expect(screen.queryByText('agent.running')).toBeNull();
    // 停止 → 卡片过期：操作按钮消失、过期文案出现
    expect(screen.queryByRole('button', { name: 'agent.approveOnce' })).toBeNull();
    expect(screen.getByText('agent.approvalExpired')).toBeTruthy();
    // 输入文本后发送按钮恢复可用
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: 'hi' } });
    expect((screen.getByRole('button', { name: 'agent.send' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('expires pending approval cards on disconnect (onclose) and unlocks send', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({
        type: 'approval_request',
        request_id: 'req1',
        tool: 'shell',
        summary: 'rm -rf /tmp/x',
        args_preview: '{}',
      });
    });
    // 断线：服务端 turn 被 drop、审批按 deny 落定；重连后历史 refetch 若失败，
    // 本地卡片不置终态会让 hasPendingInteraction 恒 true → 发送按钮永久锁死
    act(() => {
      wsInstance!.onclose?.();
    });
    // 断线 → 卡片过期：操作按钮消失、过期文案出现
    expect(screen.queryByRole('button', { name: 'agent.approveOnce' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'agent.deny' })).toBeNull();
    expect(screen.getByText('agent.approvalExpired')).toBeTruthy();
    // 输入文本后发送按钮恢复可用
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: 'hi' } });
    expect((screen.getByRole('button', { name: 'agent.send' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('renders elicitation card and submits accept with content', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // 注入 elicitation_request 帧（AskUserQuestion 单选 schema）
    act(() => {
      wsInstance!.emit({
        type: 'elicitation_request',
        request_id: 'req1',
        message: 'Choose a color',
        schema: {
          type: 'object',
          properties: {
            question_1: {
              type: 'string',
              title: 'Color',
              oneOf: [
                { const: 'red', title: 'Red' },
                { const: 'blue', title: 'Blue' },
              ],
            },
          },
          required: ['question_1'],
        },
      });
    });
    // 卡片出现（标题 + 消息 + 单选选项）
    expect(screen.getByText(/agent\.elicitationRequired/)).toBeTruthy();
    expect(screen.getByText('Choose a color')).toBeTruthy();
    expect(screen.getByRole('button', { name: /Red/ })).toBeTruthy();
    // 必填未填 → 提交禁用；选中 Red 后提交
    expect((screen.getByRole('button', { name: 'agent.elicitationSubmit' }) as HTMLButtonElement).disabled).toBe(true);
    const ws = wsInstance!;
    fireEvent.click(screen.getByRole('button', { name: /Red/ }));
    fireEvent.click(screen.getByRole('button', { name: 'agent.elicitationSubmit' }));
    expect(
      ws.sent.some(
        (s) =>
          s.includes('"type":"elicitation_response"') &&
          s.includes('"request_id":"req1"') &&
          s.includes('"action":"accept"') &&
          s.includes('"content":{"question_1":"red"}'),
      ),
    ).toBe(true);
    // 卡片变已提交：操作按钮消失、终态文案出现
    expect(screen.queryByRole('button', { name: 'agent.elicitationSubmit' })).toBeNull();
    expect(screen.getByText(/agent\.elicitationAnswered/)).toBeTruthy();
  });

  it('declines elicitation card and sends decline frame', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({
        type: 'elicitation_request',
        request_id: 'req2',
        message: 'Confirm?',
        schema: { type: 'object', properties: {}, required: [] },
      });
    });
    const ws = wsInstance!;
    fireEvent.click(screen.getByRole('button', { name: 'agent.elicitationDecline' }));
    expect(
      ws.sent.some(
        (s) =>
          s.includes('"type":"elicitation_response"') &&
          s.includes('"request_id":"req2"') &&
          s.includes('"action":"decline"'),
      ),
    ).toBe(true);
    // 卡片变已跳过：操作按钮消失、跳过徽章出现
    expect(screen.queryByRole('button', { name: 'agent.elicitationDecline' })).toBeNull();
    expect(screen.getByText(/agent\.elicitationDeclined/)).toBeTruthy();
  });

  it('locks send while elicitation pending and unlocks after done frame', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // pending elicitation：hasPendingInteraction 门控 → 发送按钮禁用（服务端挂起回合）
    act(() => {
      wsInstance!.emit({
        type: 'elicitation_request',
        request_id: 'req1',
        message: 'Fill this',
        schema: { type: 'object', properties: {}, required: [] },
      });
    });
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: 'hi' } });
    expect((screen.getByRole('button', { name: 'agent.send' }) as HTMLButtonElement).disabled).toBe(true);
    // done 帧到达（服务端 elicitation 超时按 Cancel 继续回合）→ 卡片置 cancelled、发送解锁
    act(() => {
      wsInstance!.emit({ type: 'done' });
    });
    expect(screen.getByText('agent.elicitationCancelled')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'agent.elicitationSubmit' })).toBeNull();
    expect((screen.getByRole('button', { name: 'agent.send' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('expires pending elicitation card on disconnect and unlocks send', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({
        type: 'elicitation_request',
        request_id: 'req1',
        message: 'Fill this',
        schema: { type: 'object', properties: {}, required: [] },
      });
    });
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: 'hi' } });
    expect((screen.getByRole('button', { name: 'agent.send' }) as HTMLButtonElement).disabled).toBe(true);
    // 断线：服务端 turn 被 drop、elicitation 按 Cancel 落定 → 卡片置 cancelled、发送解锁
    act(() => {
      wsInstance!.onclose?.();
    });
    expect(screen.getByText('agent.elicitationCancelled')).toBeTruthy();
    expect((screen.getByRole('button', { name: 'agent.send' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('shows mention popup on @ and sends refs with message', async () => {
    (listWorkspaceFiles as Mock).mockResolvedValue({ files: ['src/main.rs'] });
    renderChat();
    // 输入 @mai → @ 弹层出现，列出匹配文件
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: '@mai' } });
    expect(await screen.findByText('src/main.rs')).toBeTruthy();
    // 选中文件 → @query 段从文本移除，路径进引用 chip
    fireEvent.click(screen.getByText('src/main.rs'));
    expect(screen.getByText('@src/main.rs')).toBeTruthy();
    expect((screen.getByPlaceholderText('agent.inputPlaceholder') as HTMLTextAreaElement).value).toBe('');
    // 输入消息并发送 → WS 帧带 refs
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: '检查这个文件' } });
    const ws = wsInstance!;
    fireEvent.click(screen.getByRole('button', { name: 'agent.send' }));
    expect(
      ws.sent.some((s) => s.includes('"type":"user_message"') && s.includes('"refs":["src/main.rs"]')),
    ).toBe(true);
  });

  it('selects the highlighted mention item on Enter without sending', async () => {
    (listWorkspaceFiles as Mock).mockResolvedValue({ files: ['src/main.rs'] });
    renderChat();
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: '@mai' } });
    expect(await screen.findByText('src/main.rs')).toBeTruthy();
    const textarea = screen.getByPlaceholderText('agent.inputPlaceholder') as HTMLTextAreaElement;
    // 弹层打开时按 Enter → 选中高亮项，而非发送消息
    fireEvent.keyDown(textarea, { key: 'Enter' });
    expect(screen.getByText('@src/main.rs')).toBeTruthy();
    expect(textarea.value).toBe('');
    // 任何连接的 WS 实例都没有发出 user_message 帧（断言覆盖 i18n mock 引发的实例轮换）
    expect(wsInstances.every((w) => !w.sent.some((s) => s.includes('"type":"user_message"')))).toBe(true);
    // 弹层关闭
    expect(screen.queryByText('src/main.rs')).toBeNull();
  });

  it('moves highlight with ArrowDown and selects the second item on Enter', async () => {
    (listWorkspaceFiles as Mock).mockResolvedValue({ files: ['src/a.rs', 'src/b.rs'] });
    renderChat();
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: '@sr' } });
    expect(await screen.findByText('src/a.rs')).toBeTruthy();
    const textarea = screen.getByPlaceholderText('agent.inputPlaceholder') as HTMLTextAreaElement;
    // ↓ 移动高亮到第二项 → Enter 选中 src/b.rs
    fireEvent.keyDown(textarea, { key: 'ArrowDown' });
    fireEvent.keyDown(textarea, { key: 'Enter' });
    expect(await screen.findByText('@src/b.rs')).toBeTruthy();
    expect(screen.queryByText('@src/a.rs')).toBeNull();
    expect(textarea.value).toBe('');
  });

  it('closes the mention popup on Escape', async () => {
    (listWorkspaceFiles as Mock).mockResolvedValue({ files: ['src/main.rs'] });
    renderChat();
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: '@mai' } });
    expect(await screen.findByText('src/main.rs')).toBeTruthy();
    fireEvent.keyDown(screen.getByPlaceholderText('agent.inputPlaceholder'), { key: 'Escape' });
    expect(screen.queryByText('src/main.rs')).toBeNull();
  });

  it('removes a single chip independently', async () => {
    (listWorkspaceFiles as Mock).mockResolvedValue({ files: ['src/a.rs', 'src/b.rs'] });
    renderChat();
    const textarea = screen.getByPlaceholderText('agent.inputPlaceholder') as HTMLTextAreaElement;
    // 依次选择两个文件 → 两个 chip
    fireEvent.change(textarea, { target: { value: '@sr' } });
    fireEvent.click(await screen.findByText('src/a.rs'));
    fireEvent.change(textarea, { target: { value: '@sr' } });
    fireEvent.click(await screen.findByText('src/b.rs'));
    expect(screen.getByText('@src/a.rs')).toBeTruthy();
    expect(screen.getByText('@src/b.rs')).toBeTruthy();
    // 单独删除 src/a.rs 的 chip，src/b.rs 保留
    const chipA = screen.getByText('@src/a.rs');
    const removeBtn = chipA.parentElement!.querySelector('button')!;
    fireEvent.click(removeBtn);
    expect(screen.queryByText('@src/a.rs')).toBeNull();
    expect(screen.getByText('@src/b.rs')).toBeTruthy();
  });

  it('renders thought chunks as thought bubble separate from text', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '正文' });
      wsInstance!.emit({ type: 'assistant_chunk', content: '推理第一行\n隐藏的推理细节', thought: true });
      wsInstance!.emit({ type: 'assistant_chunk', content: '续正文' });
      wsInstance!.emit({ type: 'done' });
    });
    // thought 与正文是分开的气泡：正文两段各自成气泡；thought 默认折叠只显示首行预览
    expect(screen.getByText('正文')).toBeTruthy();
    expect(screen.getByText('续正文')).toBeTruthy();
    expect(screen.getByText('推理第一行')).toBeTruthy();
    expect(screen.queryByText('隐藏的推理细节')).not.toBeTruthy();
  });

  it('tool_call frame carries kind/diffs into card', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({
        type: 'tool_call', id: 'c1', name: 'Edit a.ts', tool_kind: 'edit',
        diffs: [{ path: 'a.ts', old_text: 'x', new_text: 'y' }],
      });
      wsInstance!.emit({ type: 'tool_result', id: 'c1', name: 'Edit a.ts', status: 'completed', result: 'ok' });
      wsInstance!.emit({ type: 'done' });
    });
    expect(screen.getByText('✓')).toBeTruthy();
    // 展开卡片应看到 diff（点击头部——标题归一化为 Edit，内嵌目标 a.ts 为摘要）
    fireEvent.click(screen.getByText('Edit').closest('button')!);
    expect(screen.getByText('+ y')).toBeTruthy();
  });

  it('tool_result without name falls back to id matching', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'Read x', tool_kind: 'read' });
      wsInstance!.emit({ type: 'tool_call', id: 'c2', name: 'Read y', tool_kind: 'read' });
      // c1 的结果不带 name（ACP ToolCallUpdate 常缺 title）
      wsInstance!.emit({ type: 'tool_result', id: 'c1', status: 'completed', result: 'r1' });
      wsInstance!.emit({ type: 'done' });
    });
    // c1 完成、不占用 c2 的卡片（按 id 回退：r1 落在 c1 的卡片）。
    // 两张卡片标题归一化为 Read，用内嵌目标（y）定位 c2 卡片头部
    fireEvent.click(screen.getByText('y').closest('button')!);
    // c2 卡片仍在执行中（r1 没有误挂到 c2）
    expect(screen.getAllByText('agent.toolRunning').length).toBeGreaterThan(0);
  });

  it('tool_result args backfills placeholder {} args from tool_call (claude-code late rawInput)', async () => {
    // 实测 claude-code-acp 0.66.0 帧序列：ToolCall 首帧 rawInput={}（占位），
    // 真正的参数经 ToolCallUpdate.rawInput 由 tool_result 帧携带。卡片头部摘要
    // 必须从空占位实时补出真实命令，否则「无操作内容」。
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({
        type: 'tool_call', id: 'c1', name: 'Terminal', tool_kind: 'execute',
        args: '{}', status: 'in_progress',
      });
      wsInstance!.emit({
        type: 'tool_result', id: 'c1', name: 'echo hello',
        args: '{"command":"echo hello","description":"Print hello"}', status: 'running',
      });
      wsInstance!.emit({
        type: 'tool_result', id: 'c1', status: 'completed', result: 'hello',
      });
      wsInstance!.emit({ type: 'done' });
    });
    // 摘要必须显示真实命令（args 已从 {} 覆盖为真实参数）——execute 卡片从
    // args 提取 command 作为头部摘要
    expect(screen.getByText('echo hello')).toBeTruthy();
    // 已完成
    expect(screen.getByText('✓')).toBeTruthy();
    // 展开卡片应看到完整 args（含 description 字段，证明不是 {} 占位）
    fireEvent.click(screen.getByText('Terminal'));
    expect(screen.getByText(/"command":"echo hello"/)).toBeTruthy();
  });

  it('tool_result without args preserves existing card args', async () => {
    // tool_call 首帧已带真实 args（无占位），tool_result 不带 args（只带结果）：
    // 不能清空已有 args。
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({
        type: 'tool_call', id: 'c1', name: 'shell', tool_kind: 'execute',
        args: '{"cmd":"ls"}', status: 'in_progress',
      });
      wsInstance!.emit({ type: 'tool_result', id: 'c1', status: 'completed', result: 'ok' });
      wsInstance!.emit({ type: 'done' });
    });
    // shell 归一化为 Terminal，点击头部展开
    fireEvent.click(screen.getByText('Terminal'));
    expect(screen.getByText(/"cmd":"ls"/)).toBeTruthy();
  });

  it('plan frame updates the last plan bubble in place', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'plan', entries: [{ content: '甲', status: 'in_progress' }] });
      wsInstance!.emit({ type: 'plan', entries: [{ content: '甲', status: 'completed' }] });
      wsInstance!.emit({ type: 'done' });
    });
    // 只有一条 plan 气泡，内容为最新状态
    expect(screen.getAllByText('甲')).toHaveLength(1);
    expect(screen.getByText('✓')).toBeTruthy();
  });

  it('usage frame does not render or crash', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'usage', used: 100, size: 200000 });
      wsInstance!.emit({ type: 'done' });
    });
    expect(screen.queryByText(/100/)).not.toBeTruthy();
  });

  it('restores acp history: tool diff card, thought, plan (last only)', async () => {
    const acpCall = JSON.stringify([{
      id: 'c1', name: 'Edit a.ts', arguments: '{"file_path":"a.ts"}',
      tool_kind: 'edit', diffs: [{ path: 'a.ts', old_text: 'x', new_text: 'y' }],
      locations: [{ path: 'a.ts', line: 1 }],
    }]);
    (listAgentMessages as Mock).mockResolvedValue([
      { id: 'm1', session_id: 's1', role: 'user', content: '改一下', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-08' },
      { id: 'm2', session_id: 's1', role: 'assistant', content: '想一下\n隐藏的推理', tool_calls: null, tool_call_id: null, name: 'thought', kind: 'message', created_at: '2026-08-08' },
      { id: 'm3', session_id: 's1', role: 'assistant', content: '', tool_calls: acpCall, tool_call_id: 'c1', name: 'Edit a.ts', kind: 'tool_calls', created_at: '2026-08-08' },
      { id: 'm4', session_id: 's1', role: 'assistant', content: 'done ok', tool_calls: null, tool_call_id: 'c1', name: 'Edit a.ts', kind: 'tool_result', created_at: '2026-08-08' },
      { id: 'm5', session_id: 's1', role: 'assistant', content: JSON.stringify([{ content: '旧计划', status: 'pending' }]), tool_calls: null, tool_call_id: null, name: 'plan', kind: 'message', created_at: '2026-08-08' },
      { id: 'm6', session_id: 's1', role: 'assistant', content: JSON.stringify([{ content: '新计划', status: 'completed' }]), tool_calls: null, tool_call_id: null, name: 'plan', kind: 'message', created_at: '2026-08-08' },
      { id: 'm7', session_id: 's1', role: 'assistant', content: '已完成', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-08' },
    ]);
    renderChat();
    expect(await screen.findByText('改一下')).toBeTruthy();
    expect(await screen.findByText('已完成')).toBeTruthy();
    // 只渲染最后一条 plan
    expect(screen.queryByText('旧计划')).not.toBeTruthy();
    expect(await screen.findByText('新计划')).toBeTruthy();
    // tool 卡片带完成态；标题归一化为 Edit，展开见 diff
    expect(await screen.findByText('Edit')).toBeTruthy();
    await act(async () => {
      screen.getByText('Edit').closest('button')!.click();
    });
    expect(screen.getByText('+ y')).toBeTruthy();
    // thought 折叠：只显示首行预览，完整内容（后续行）不可见
    expect(screen.getByText('想一下')).toBeTruthy();
    expect(screen.queryByText('隐藏的推理')).not.toBeTruthy();
  });

  it('reloads when cached history was empty but refetch returns rows', async () => {
    // loadedRef 自愈：首轮装载空历史后，refetch 拿到非空历史必须重新装载。
    // 确定性覆盖自愈分支：渲染前 setQueryData 预置空缓存，保证首轮 history
    // effect 必以空历史同步运行（loadedRef=true 且 items 空）。随后 invalidate
    // 触发的 refetch 拿到非空历史 → 走自愈重装路径。旧写法先渲染再等首轮空
    // fetch 落定，jsdom/vitest 时序下 refetch 可能先于首轮装载发生，走的是
    // 「首次装载」路径——旧守卫 `if (loadedRef.current) return;` 下用例同样
    // PASS，未确定性覆盖自愈分支。
    (listAgentMessages as Mock).mockResolvedValue([]);
    const qc = new QueryClient({
      defaultOptions: { queries: { retry: false, refetchOnMount: false } },
    });
    qc.setQueryData(['agent-messages', 's1'], []);
    render(
      <QueryClientProvider client={qc}>
        <ChatStream sessionId="s1" workspaceId="w1" model="" onModelChange={vi.fn()} />
      </QueryClientProvider>
    );
    (listAgentMessages as Mock).mockResolvedValue([
      { id: 'm1', session_id: 's1', role: 'user', content: '迟到的历史', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-08' },
    ]);
    await act(async () => {
      await qc.invalidateQueries({ queryKey: ['agent-messages', 's1'] });
    });
    expect(await screen.findByText('迟到的历史')).toBeTruthy();
  });

  it('applies session_state frame to config options state', () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // 注入 session_state 帧：mode 项 currentValue=plan（options 含 {value:plan,name:Plan}）
    act(() => {
      wsInstance!.emit({
        type: 'session_state',
        options: [
          {
            id: 'mode',
            name: 'Mode',
            category: 'mode',
            type: 'select',
            currentValue: 'plan',
            options: [{ value: 'plan', name: 'Plan' }],
          },
        ],
      });
    });
    // 发送按钮左侧的 Mode 快捷按钮显示当前值 "Plan"
    const modeBtn = screen.getByRole('button', { name: 'agent.configMode' });
    expect(modeBtn).toBeTruthy();
    expect(modeBtn.textContent).toContain('Plan');
    // mode 项被过滤，不进左侧统一菜单：展开菜单后菜单内容里无 "Mode" 项
    fireEvent.pointerDown(screen.getByRole('button', { name: 'agent.sessionSettings' }));
    // 菜单已打开（ModelPicker 重构后模型区不再渲染 agent.model 标签，改用 role=menu 判定）
    expect(screen.getByRole('menu')).toBeTruthy();
    expect(screen.queryByText('Mode')).toBeNull();
  });

  it('sendConfigOption sends set_config_option frame with optimistic update', () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // 注入 session_state：effort 项 currentValue=medium（options 含 high）
    act(() => {
      wsInstance!.emit({
        type: 'session_state',
        options: [
          {
            id: 'effort',
            name: 'Effort',
            category: 'thought_level',
            type: 'select',
            currentValue: 'medium',
            options: [
              { value: 'low', name: 'Low' },
              { value: 'medium', name: 'Medium' },
              { value: 'high', name: 'High' },
            ],
          },
        ],
      });
    });
    const effortBtn = screen.getByRole('button', { name: 'agent.configEffort' });
    expect(effortBtn.textContent).toContain('Medium');
    // 展开 Effort 快捷菜单 → 点击 "High"（点击前捕获当前活跃连接：乐观更新后
    // ChatStream 重渲染会轮换 WS 实例，帧发在点击时刻的活跃连接上）
    fireEvent.pointerDown(effortBtn);
    const ws = wsInstance!;
    fireEvent.click(screen.getByText('High'));
    // 发送 set_config_option 帧（最后一条）
    expect(ws.sent[ws.sent.length - 1]).toBe(
      '{"type":"set_config_option","config_id":"effort","value":"high"}',
    );
    // 乐观更新：按钮文本立即变为 "High"（生效确认以服务端回推帧为准）
    expect(screen.getByRole('button', { name: 'agent.configEffort' }).textContent).toContain('High');
  });

  it('rolls back optimistic config option on 设置失败 error frame', () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // 注入 session_state：effort 项 currentValue=medium
    act(() => {
      wsInstance!.emit({
        type: 'session_state',
        options: [
          {
            id: 'effort',
            name: 'Effort',
            category: 'thought_level',
            type: 'select',
            currentValue: 'medium',
            options: [
              { value: 'low', name: 'Low' },
              { value: 'medium', name: 'Medium' },
              { value: 'high', name: 'High' },
            ],
          },
        ],
      });
    });
    // 展开 Effort 快捷菜单 → 点击 High：乐观更新使按钮文本立即变 High
    fireEvent.pointerDown(screen.getByRole('button', { name: 'agent.configEffort' }));
    fireEvent.click(screen.getByText('High'));
    expect(screen.getByRole('button', { name: 'agent.configEffort' }).textContent).toContain('High');
    // 服务端回「设置失败」error 帧（config_id 失效/agent 退出等）：
    // 乐观值从未生效，应回滚到发送前快照 Medium，而非停留在假性 High
    act(() => {
      wsInstance!.emit({ type: 'error', message: '设置失败: unknown config option: effort' });
    });
    expect(screen.getByRole('button', { name: 'agent.configEffort' }).textContent).toContain('Medium');
    // 错误气泡照常追加（用户可见失败原因）
    expect(screen.getByText(/设置失败/)).toBeTruthy();
  });

  it('hides Mode/Effort buttons for non-ACP sessions (no session_state)', () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // 不注入 session_state：configOptions 为空 → 快捷按钮不渲染
    expect(screen.queryByRole('button', { name: 'agent.configMode' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'agent.configEffort' })).toBeNull();
  });

  it('shows disabled Effort placeholder when agent reported options without effort', () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // 注入 session_state：只有 mode 项（无 thought_level）→ 当前模型不支持 Effort
    act(() => {
      wsInstance!.emit({
        type: 'session_state',
        options: [
          {
            id: 'mode',
            name: 'Mode',
            category: 'mode',
            type: 'select',
            currentValue: 'plan',
            options: [{ value: 'plan', name: 'Plan' }],
          },
        ],
      });
    });
    // Mode 快捷按钮正常显示当前值
    expect(screen.getByRole('button', { name: 'agent.configMode' }).textContent).toContain('Plan');
    // Effort 占位按钮存在且禁用（title 提示原因）
    const effortBtn = screen.getByRole('button', { name: 'agent.configEffort' });
    expect(effortBtn).toBeTruthy();
    expect((effortBtn as HTMLButtonElement).disabled).toBe(true);
    expect(effortBtn.getAttribute('title')).toBe('agent.configOptionUnsupported');
  });

  it('nests subagent events inside the parent Task card (tool/text/result)', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      // 父 Task 卡（is_subagent=true）+ 子工具 + 子文本 + 子结果 + 父结果
      wsInstance!.emit({
        type: 'tool_call', id: 'task1', name: 'Task',
        args: '{"description":"调研登录 bug","subagent_type":"general-purpose"}',
        is_subagent: true, status: 'in_progress',
      });
      wsInstance!.emit({
        type: 'tool_call', id: 'c1', name: 'Read x', tool_kind: 'read',
        parent_tool_call_id: 'task1', status: 'in_progress',
      });
      wsInstance!.emit({
        type: 'assistant_chunk', content: '子代理文本', parent_tool_call_id: 'task1', final: true,
      });
      wsInstance!.emit({
        type: 'tool_result', id: 'c1', name: 'Read x', result: 'fn main(){}',
        parent_tool_call_id: 'task1', status: 'completed',
      });
      wsInstance!.emit({ type: 'tool_result', id: 'task1', name: 'Task', result: '调研完成', status: 'completed' });
      wsInstance!.emit({ type: 'done' });
    });
    // 子 agent 卡头部显示 description（非 toolName "Task"）
    expect(await screen.findByText('调研登录 bug')).toBeTruthy();
    // 父卡已完成（✓ 徽章）
    expect(screen.getByText('✓')).toBeTruthy();
    // 默认折叠：子项不可见；展开父卡
    act(() => {
      screen.getByText('调研登录 bug').closest('button')!.click();
    });
    // 子工具卡（Read）与子文本都嵌套在父卡内
    expect(screen.getByText('Read')).toBeTruthy();
    expect(screen.getByText('子代理文本')).toBeTruthy();
    // 父卡自身 toolResult（Task 最终结果）在展开态末尾展示
    expect(screen.getByText('调研完成')).toBeTruthy();
    // 展开子工具卡看结果
    fireEvent.click(screen.getByText('Read').closest('button')!);
    expect(screen.getByText(/fn main\(\)/)).toBeTruthy();
    // 顶层只有一张 Read 卡（子卡嵌套，未重复渲染）
    expect(screen.getAllByText('Read')).toHaveLength(1);
  });

  it('merges streamed subagent text chunks into a single nested bubble', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'task1', name: 'Task', is_subagent: true, status: 'in_progress' });
      wsInstance!.emit({ type: 'assistant_chunk', content: '你好', parent_tool_call_id: 'task1' });
      wsInstance!.emit({ type: 'assistant_chunk', content: '，世界', parent_tool_call_id: 'task1', final: true });
      wsInstance!.emit({ type: 'done' });
    });
    act(() => {
      // 父卡无 args → 头部回退 toolName "Task"
      screen.getByText('Task').closest('button')!.click();
    });
    // 两个 chunk 合并为一个气泡，不碎片化
    expect(screen.getByText('你好，世界')).toBeTruthy();
    expect(screen.queryByText('你好')).toBeNull();
  });

  it('attaches orphan child events when the parent card arrives later (no loss, no dup)', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // 子事件先于父卡到达（时序异常）：工具卡进 pending，文本攒批
    act(() => {
      wsInstance!.emit({
        type: 'tool_call', id: 'c1', name: 'Read x', tool_kind: 'read',
        parent_tool_call_id: 'task1', status: 'in_progress',
      });
      wsInstance!.emit({
        type: 'assistant_chunk', content: '先到的子文本', parent_tool_call_id: 'task1', final: false,
      });
    });
    // 父卡后到：挂载已缓存的子项
    act(() => {
      wsInstance!.emit({
        type: 'tool_call', id: 'task1', name: 'Task',
        args: '{"description":"迟到的父卡"}', is_subagent: true, status: 'in_progress',
      });
      wsInstance!.emit({ type: 'done' });
    });
    expect(await screen.findByText('迟到的父卡')).toBeTruthy();
    act(() => {
      screen.getByText('迟到的父卡').closest('button')!.click();
    });
    // 缓存的子工具卡与文本都挂载进父卡 children（无重复、无丢失）
    expect(screen.getByText('Read')).toBeTruthy();
    expect(screen.getByText('先到的子文本')).toBeTruthy();
    expect(screen.getAllByText('Read')).toHaveLength(1);
  });

  it('flushes un-mounted orphan events to the main stream on done', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      // 父卡从未出现：子文本 final 时进 pending，done 时平铺回主流
      wsInstance!.emit({
        type: 'assistant_chunk', content: '孤儿子文本', parent_tool_call_id: 'ghost', final: true,
      });
      wsInstance!.emit({ type: 'done' });
    });
    expect(await screen.findByText('孤儿子文本')).toBeTruthy();
  });

  it('keeps parallel subagents in independent lanes', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({
        type: 'tool_call', id: 'taskA', name: 'Task', args: '{"description":"A 任务"}',
        is_subagent: true, status: 'in_progress',
      });
      wsInstance!.emit({
        type: 'tool_call', id: 'taskB', name: 'Task', args: '{"description":"B 任务"}',
        is_subagent: true, status: 'in_progress',
      });
      wsInstance!.emit({
        type: 'tool_call', id: 'a1', name: 'Read alpha', tool_kind: 'read', parent_tool_call_id: 'taskA',
      });
      wsInstance!.emit({
        type: 'tool_call', id: 'b1', name: 'Read beta', tool_kind: 'read', parent_tool_call_id: 'taskB',
      });
      wsInstance!.emit({ type: 'assistant_chunk', content: 'A 的文本', parent_tool_call_id: 'taskA', final: true });
      wsInstance!.emit({ type: 'assistant_chunk', content: 'B 的文本', parent_tool_call_id: 'taskB', final: true });
      wsInstance!.emit({ type: 'done' });
    });
    expect(await screen.findByText('A 任务')).toBeTruthy();
    expect(screen.getByText('B 任务')).toBeTruthy();
    // 分别展开两张父卡：各自的子工具/文本只出现在自己卡内
    act(() => {
      screen.getByText('A 任务').closest('button')!.click();
      screen.getByText('B 任务').closest('button')!.click();
    });
    expect(screen.getByText('alpha')).toBeTruthy();
    expect(screen.getByText('beta')).toBeTruthy();
    expect(screen.getByText('A 的文本')).toBeTruthy();
    expect(screen.getByText('B 的文本')).toBeTruthy();
  });

  it('renders non-subagent tool frames flat (unsupported engines degrade silently)', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      // 无 parent_tool_call_id / is_subagent 字段：完全走既有平铺行为
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'Read x', tool_kind: 'read', status: 'in_progress' });
      wsInstance!.emit({ type: 'tool_result', id: 'c1', name: 'Read x', result: 'ok', status: 'completed' });
      wsInstance!.emit({ type: 'done' });
    });
    expect(await screen.findByText('Read')).toBeTruthy();
    // 顶层普通工具卡：非子 agent 卡，不渲染 SubagentTaskCard 头
    expect(screen.queryByText('agent.subagent')).toBeNull();
    fireEvent.click(screen.getByText('Read').closest('button')!);
    expect(screen.getByText('ok')).toBeTruthy();
  });
});
